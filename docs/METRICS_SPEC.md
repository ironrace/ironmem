# ironmem Metrics Spec (v0, Phase 0)

> **Status:** FROZEN 2026-06-11. No instrumentation code (migration 008, `LlmClient`
> usage plumbing, MCP sizing, occupancy sampling, task tagging, or `ironmem report`)
> may land until this document is merged. After merge, every counter name, unit,
> source, destination, estimation rule, and reporting query is fixed here. If a later
> change to the instrumentation requires a spec change, the change MUST be **dated,
> justified, and appended to §12 (Amendments)** — never silently absorbed into the body.
>
> The freeze hash is recorded in this PR's body and in the merge tag annotation, **not**
> inside this file (embedding a hash would change the bytes it certifies).
>
> **Purpose:** This document is the contract for measuring whether ironmem + planning
> discipline reaches merged-and-passing with fewer rework loops and lower total tokens
> than superpowers alone. It is the Phase 0 deliverable of the 20-PR improvement roadmap
> (issues #79–#98); issue #79 is closed by merging it. Every later metrics PR in that
> roadmap cites a section here in its description.

---

## 1. Purpose & thesis

### 1.1 Falsifiable claim

> ironmem + planning discipline reaches **merged-and-passing** with **fewer rework loops**
> and **lower total tokens per task** than superpowers-alone, and stays smart in long
> sessions because durable server-side state makes `/clear` cheap.

The claim is falsifiable: §11 defines a two-arm A/B protocol whose result either shows a
confidence-qualified reduction in tokens-to-done and rework loops for the ironmem arm, or
it does not. "Stays smart in long sessions" is operationalized as **occupancy** (§8) staying
below threshold across a full task without loss of outcome quality.

### 1.2 What this spec is *not*

It is not an optimization target generator. The primary metric (§2) is the only headline
number. Per-call counters (§5) exist to **diagnose** where tokens go, never to be minimized
in isolation — see the Goodhart guards in §9.

---

## 2. Primary metric: tokens-to-done per task

### 2.1 Definition

**Primary metric = total tokens consumed to take one task from start to "done", attributed
to that task, decomposed by phase.**

```
tokens_to_done(task) = Σ over all token_usage rows where row.task_key = task
                       of (input_tokens + output_tokens
                           + cache_creation_input_tokens + cache_read_input_tokens)
```

Reported as a single number per task **and** broken out by phase bucket (§3). Cache-read
tokens are included because they are real context the model processed; the cache discount
is a **cost** concern (§7), not a **token-count** concern.

### 2.2 "Done"

A task is **done** iff its pull request is **merged AND CI is green on the merge commit**.
A task that is closed without merge, or abandoned, is `failed` / `abandoned` (§4) and is
**excluded from headline tokens-to-done averages** — it is reported separately as a
non-completion, with its accumulated tokens, so that "cheap because it gave up" can never
masquerade as "cheap because it was efficient".

### 2.3 Task identity (`task_key`)

Exactly one of, in priority order:

1. **`collab_session_id`** — when the work ran under a `/collab` session, the session id
   is the task key. One collab session = one task = one PR (the collab state machine is
   one-task-one-PR by design, terminating at `CodingComplete` with a single `pr_url`).
2. **Explicit `task_tag`** — for non-collab work (e.g. plain TDD sessions like #79, #85,
   #86), a caller-supplied tag set via the status tool. Absent a tag, the rows are recorded
   with `task_key = NULL` and are **diagnostic-only**: they never enter a per-task headline
   number, because there is no defensible task boundary to attribute them to.

`task_key` is therefore `COALESCE(collab_session_id, task_tag)`. A row with neither is
counted only in global/source rollups, never in per-task reporting.

---

## 3. Phase decomposition

### 3.1 Buckets

Every token_usage row is attributed to exactly one of four phase buckets:

| Bucket     | Meaning                                                        |
|------------|---------------------------------------------------------------|
| `planning` | Producing and agreeing the plan before implementation starts. |
| `impl`     | Writing code / tests to satisfy the agreed plan.              |
| `review`   | First-pass review of completed implementation.               |
| `rework`   | Changes driven by review findings (the loop the thesis targets). |

A fifth value, `other`, exists for rows that cannot be attributed (no active phase context,
e.g. ad-hoc status calls). `other` is reported but excluded from the planning/impl/review/rework
breakdown denominators.

### 3.2 Mapping from collab session phase

Collab phase attribution derives from the **session's `Phase`**, not from raw message
`topic` strings. This is deliberate: collab message topics are **reused across phases**
(`crates/ironmem/src/mcp/tools/collab_session.rs:88-91` — "The topic string `final` is
intentionally reused across versions; dispatch happens on the current phase inside
`build_collab_event`"). A topic-keyed table would misbucket every reused topic (`final`,
bare `review`), corrupting the phase decomposition. The authoritative, already-tracked key
is `session.phase` (the `Phase` enum in `crates/ironmem/src/collab/phase.rs`):

| `Phase` variant                | Phase bucket | Notes                                              |
|--------------------------------|--------------|----------------------------------------------------|
| `PlanParallelDrafts`           | `planning`   | Blind parallel plan drafts.                        |
| `PlanSynthesisPending`         | `planning`   | Canonical plan synthesis.                          |
| `PlanCodexReviewPending`       | `planning`   | Plan-review assistance, pre-implementation.        |
| `PlanClaudeFinalizePending`    | `planning`   | Plan finalization, pre-implementation.             |
| `PlanLocked`                   | `planning`   | Plan accepted; task-list assembly, pre-impl.       |
| `CodeImplementPending`         | `impl`       | Implementation underway.                           |
| `CodeReviewLocalPending`       | `review`     | Local `/ultrareview-local` first pass.             |
| `CodeReviewFixGlobalPending`   | `rework`     | Global review-fix pass (review-driven changes).    |
| `CodeReviewFinalPending`       | `review`     | Final review audit from drawers + server state.    |
| `CodingComplete`               | `other`      | Terminal; no work attributed.                      |
| `CodingFailed`                 | `other`      | Terminal failure; no work attributed.              |

**Attribution rule:** a token_usage row recorded while a collab session is active is tagged
with the phase bucket of the session's **current `Phase`** at the time the row is recorded,
read from the session record (`record.session.phase`). This is the single source of truth —
attribution keys on server-side phase state, never on the raw topic string, and there is no
heuristic re-classification after the fact. Message `topic` strings (including the reused
`final` and `review`, and `failure_report`) are not used for bucketing.

### 3.3 Non-collab attribution

For non-collab tagged tasks there is no topic stream. Rows are attributed by an explicit
phase set on the `MetricsContext` (default `impl`). Plain TDD sessions that never set a phase
record all rows as `impl`; this is acceptable because those tasks (spec docs, mechanical CPU
fixes) have no meaningful planning/review split to measure.

---

## 4. Iteration counters

Iteration counts are first-class columns on `task_outcomes` (§5), not derived at report time.
A higher planning/review token spend that *reduces* these counters is a **win**, not a
regression — that is the whole thesis.

| Counter        | Unit   | Increment rule                                                                 |
|----------------|--------|--------------------------------------------------------------------------------|
| `review_rounds`| count  | +1 each time a review phase begins after an implementation or rework phase (each entry into `review_local` / `final_review` following `impl`/`rework`). |
| `fix_commits`  | count  | +1 per commit made during a `rework` phase (review-driven fix commits only — not original implementation commits). |
| `handoffs`     | count  | +1 each time session ownership transfers via the (future) `session_handoff` lease, or a successor session claims a new generation. 0 until that machinery exists (PR 13/15). |

Increment ownership: the collab state machine increments `review_rounds` on phase transition;
`fix_commits` is incremented by the metrics layer when a commit lands while the active phase
is `rework`; `handoffs` is incremented by the handoff tool. All three default to 0 and are
monotonic within a task.

---

## 5. Counter catalog

All counters land in one of four tables (created by migration 008, PR 2). `collab_session_id`
is a **soft foreign key** (no `REFERENCES`) so metrics survive collab-session pruning.

### 5.1 `token_usage` — per-call token rows

| Column                          | Unit    | Source                                  | Destination               |
|---------------------------------|---------|-----------------------------------------|---------------------------|
| `source`                        | enum    | code path                               | `token_usage.source` ∈ {`llm_rerank`,`pref_extract`,`transcript`,`mcp_response`} |
| `harness`                       | enum    | hook `--harness` / MCP context          | `token_usage.harness` ∈ {`claude`,`codex`} |
| `model`                         | string  | API/CLI response                        | `token_usage.model` (pinned IDs per §7) |
| `session_id`                    | string  | hook / MCP context                      | `token_usage.session_id`  |
| `collab_session_id`             | string  | active collab session                   | `token_usage.collab_session_id` (soft FK) |
| `collab_phase`                  | enum    | §3 session-`Phase` rule                 | `token_usage.collab_phase` ∈ {`planning`,`impl`,`review`,`rework`,`other`} |
| `task_tag`                      | string  | status-tool arg                         | `token_usage.task_tag`    |
| `input_tokens`                  | tokens  | API `usage` / CLI JSON / chars estimate | `token_usage.input_tokens` |
| `output_tokens`                 | tokens  | API `usage` / CLI JSON / chars estimate | `token_usage.output_tokens` |
| `cache_creation_input_tokens`   | tokens  | API `usage` (0 if absent)               | `token_usage.cache_creation_input_tokens` |
| `cache_read_input_tokens`       | tokens  | API `usage` (0 if absent)               | `token_usage.cache_read_input_tokens` |
| `estimated`                     | bool    | §6 fallback flag                        | `token_usage.estimated`   |
| `chars`                         | count   | serialized byte/char length             | `token_usage.chars`       |
| `cost_usd`                      | usd     | §7 table × tokens                       | `token_usage.cost_usd`    |
| `ts`                            | iso8601 | clock                                   | `token_usage.ts`          |

Indexes: `(task_tag, ts)` and `(collab_session_id, collab_phase)`.

### 5.2 `occupancy_samples` — context-window pressure

| Column                    | Unit    | Source                          | Destination |
|---------------------------|---------|---------------------------------|-------------|
| `harness`                 | enum    | hook `--harness`                | `occupancy_samples.harness` |
| `session_id`              | string  | hook stdin                      | `occupancy_samples.session_id` |
| `workspace_root`          | string  | hook stdin                      | `occupancy_samples.workspace_root` |
| `hook_event`              | enum    | hook name                       | `occupancy_samples.hook_event` ∈ {`session-start`,`session-stop`,`precompact`,`user-prompt-submit`} |
| `input_tokens`            | tokens  | last assistant transcript msg   | `occupancy_samples.input_tokens` |
| `cache_read_input_tokens` | tokens  | last assistant transcript msg   | `occupancy_samples.cache_read_input_tokens` |
| `context_window`          | tokens  | `IRONMEM_CONTEXT_WINDOW`        | `occupancy_samples.context_window` |
| `occupancy_pct`           | ratio   | §8 formula                      | `occupancy_samples.occupancy_pct` |
| `ts`                      | iso8601 | clock                           | `occupancy_samples.ts` |

### 5.3 `session_summary` — per-session rollup

| Column                | Unit    | Source                        | Destination |
|-----------------------|---------|-------------------------------|-------------|
| `session_id` (PK)     | string  | hook                          | `session_summary.session_id` |
| `harness`             | enum    | hook                          | `session_summary.harness` |
| `workspace_root`      | string  | hook                          | `session_summary.workspace_root` |
| `started_at`          | iso8601 | first hook event              | `session_summary.started_at` |
| `ended_at`            | iso8601 | session-stop                  | `session_summary.ended_at` |
| `peak_occupancy_pct`  | ratio   | max over samples              | `session_summary.peak_occupancy_pct` |
| `total_input_tokens`  | tokens  | Σ                             | `session_summary.total_input_tokens` |
| `total_output_tokens` | tokens  | Σ                             | `session_summary.total_output_tokens` |
| `mcp_chars_served`    | count   | Σ `write_response` lengths    | `session_summary.mcp_chars_served` |
| `compactions`         | count   | precompact event count        | `session_summary.compactions` |

### 5.4 `task_outcomes` — per-task outcome + iteration counters

| Column              | Unit    | Source                  | Destination |
|---------------------|---------|-------------------------|-------------|
| `task_tag` (UNIQUE) | string  | collab_start            | `task_outcomes.task_tag` |
| `collab_session_id` | string  | collab_start            | `task_outcomes.collab_session_id` |
| `started_at`        | iso8601 | collab_start            | `task_outcomes.started_at` |
| `done_at`           | iso8601 | end/PR                  | `task_outcomes.done_at` |
| `outcome`           | enum    | end/PR                  | `task_outcomes.outcome` ∈ {`merged`,`failed`,`abandoned`} |
| `review_rounds`     | count   | §4                      | `task_outcomes.review_rounds` |
| `fix_commits`       | count   | §4                      | `task_outcomes.fix_commits` |
| `handoffs`          | count   | §4                      | `task_outcomes.handoffs` |
| `pr_url`            | string  | PR creation             | `task_outcomes.pr_url` |

In PR 05 / issue #83, `status(set_task_tag=...)` is token-usage-only: it
annotates subsequent non-collab `token_usage` rows with `task_tag` and the
default `impl` bucket, but it does not create or complete a `task_outcomes`
row. Non-collab task lifecycle rows require a later explicit lifecycle API.

---

## 6. Estimation rules

### 6.1 When estimation is permitted

Token counts come from a real `usage` object whenever one exists:

- **Anthropic API backend** (`AnthropicApiClient`): the `usage` object — `input_tokens`,
  `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`.
- **Claude CLI backend** (`ClaudeCliClient`): the `usage` / `total_cost_usd` fields of
  `claude --output-format json`.
- **MCP responses** and any path with no model `usage`: estimation is permitted.

### 6.2 The estimate

```
estimated_tokens = ceil(chars / 4)
```

`chars / 4` is the canonical rough token estimate for English-weighted text. When a row is
populated by estimate rather than a real `usage` object, `estimated = 1` (true) and `chars`
records the measured length the estimate derived from.

### 6.3 `estimated` flag semantics — estimates never in headline numbers

- A row with `estimated = 1` is a **lower-confidence diagnostic**.
- The primary metric (§2) and the A/B headline deltas (§11) are computed from
  **measured rows only** (`estimated = 0`), OR they report the measured and estimated
  splits separately. An estimated token is never silently summed into a measured headline.
- `ironmem report` (§10) always surfaces the measured-vs-estimated split so a reader can
  see how much of a number is real.

> Rationale (memory `feedback_provbench_*` discipline): a benchmark that mixes estimated and
> measured numbers drifts to whatever the estimate says. Keep them separable at the source.

---

## 7. Cost table

Cost is derived, never measured directly: `cost_usd = Σ (tokens_of_kind × rate_of_kind)`.
Rates are **per million tokens (MTok)**. Cache-read input bills at ~0.1× input; cache-creation
(write) bills at ~1.25× input for the 5-minute TTL. These multipliers are applied to the
input rate of the row's model.

### 7.1 Pinned model rates (revision 2026-06-11)

| Model ID (pinned)     | Input $/MTok | Output $/MTok | Role in worker-dispatch tiers |
|-----------------------|--------------|---------------|-------------------------------|
| `claude-fable-5`      | 10.00        | 50.00         | Planning-level turns (max effort). |
| `claude-opus-4-8`     | 5.00         | 25.00         | Review turns.                 |
| `claude-opus-4-7`     | 5.00         | 25.00         | (legacy review tier)          |
| `claude-sonnet-4-6`   | 3.00         | 15.00         | Mechanical turns / default.   |
| `claude-haiku-4-5`    | 1.00         | 5.00          | Cheap worker turns.           |

Effective per-token rate for cache kinds, derived from the input rate `R_in`:
`cache_read = 0.1 × R_in`, `cache_creation (5m TTL) = 1.25 × R_in`.

### 7.2 Codex (xhigh) — external provider, tracked separately

Codex-side tokens are recorded with `harness = codex` for completeness, but **Codex cost is
not priced from this table** — it is a different provider and its dominant cost lives inside
Codex, not in the MCP round-trip (verified: a trivial test took 394s at low reasoning with
cost dominated by Codex internals). Codex `cost_usd` is left NULL unless a Codex-native cost
figure is available; Codex rows are excluded from Anthropic-cost rollups and flagged in
reporting.

### 7.3 Revision log

| Date       | Change                                                            |
|------------|------------------------------------------------------------------|
| 2026-06-11 | Initial table. Rates from the Anthropic model catalog as of this date (Fable 5 $10/$50, Opus 4.8 $5/$25, Sonnet 4.6 $3/$15, Haiku 4.5 $1/$5). Pinned model IDs match the worker-per-turn dispatch tiers. |

Any rate change or new model is a **dated row here** plus a §12 amendment.

---

## 8. Occupancy

### 8.1 Definition

Occupancy is the fraction of the context window in use, sampled from the **last assistant
message** of the session transcript:

```
occupancy_pct = (input_tokens + cache_read_input_tokens) / context_window
```

where `input_tokens` and `cache_read_input_tokens` are read from the `usage` object of the
last assistant message in the transcript JSONL (the hook already reverse-scans the transcript;
`crates/ironmem/src/hook.rs`), and `context_window` defaults to **200000** tokens, overridable
via `IRONMEM_CONTEXT_WINDOW`.

> Note: `cache_creation_input_tokens` and `output_tokens` are excluded from the occupancy
> numerator — occupancy measures *resident prompt context*, which is input + cache-read, not
> what was generated or freshly written to cache this turn.

### 8.2 Sampling points

| Hook event           | When sampled                                                  |
|----------------------|---------------------------------------------------------------|
| `session-start`      | Baseline at session open.                                     |
| `session-stop`       | Final occupancy at session close.                             |
| `precompact`         | Occupancy just before a compaction (also increments `compactions`). |
| `user-prompt-submit` | (PR 10) sampled in the prompt hook, subject to the 150ms budget. |

`peak_occupancy_pct` on `session_summary` is the max over all samples for the session.

### 8.3 Kill switch & tunables

- `IRONMEM_CONTEXT_WINDOW` — window size (default 200000).
- `IRONMEM_METRICS` — global metrics kill switch; when disabled, no rows are written.

> Window-size note: 200000 is a conservative baseline. Models with larger effective windows
> (e.g. Opus 4.8 / Fable 5 at 1M) report inflated `occupancy_pct` (possibly > 1.0) unless
> `IRONMEM_CONTEXT_WINDOW` is set to the harness's effective window. Occupancy is a relative
> health signal (§9.5), so a fixed conservative window suffices for trend detection, but
> per-harness tuning is recommended before treating absolute percentages as thresholds (PR 15).

---

## 9. Goodhart guards — what is explicitly NOT optimized

1. **Per-call counters are diagnostics, never targets.** `token_usage.source`-level numbers
   (rerank tokens, pref-extract tokens, mcp_response chars) exist to *locate* spend. Reducing
   any one of them is only a win if tokens-to-done per task (§2) does not rise and outcome
   quality does not fall.
2. **Planning/review tokens are not waste.** More `planning`/`review` tokens that reduce
   `rework` tokens and `review_rounds` is the thesis succeeding. The headline must never be
   "minimize planning tokens".
3. **Estimated tokens never enter headline numbers** (§6.3).
4. **Non-completion is never cheap.** `failed`/`abandoned` tasks are excluded from
   tokens-to-done averages and reported as non-completions with their spend (§2.2), so giving
   up cannot register as efficiency.
5. **Occupancy is a health signal, not a score to minimize.** Low occupancy achieved by
   dropping load-bearing context (and then doing more rework) is a loss, captured by
   tokens-to-done, not a win.
6. **Codex tokens are not mixed into Anthropic cost** (§7.2).

---

## 10. Canonical reporting SQL

`ironmem report` (PR 6) implements these queries verbatim. They are the contract; the CLI
output formats them but does not change their semantics.

### 10.1 Tokens-to-done per task, by phase (measured only)

```sql
SELECT
  COALESCE(collab_session_id, task_tag)            AS task_key,
  collab_phase,
  SUM(input_tokens + output_tokens
      + cache_creation_input_tokens
      + cache_read_input_tokens)                   AS tokens,
  SUM(cost_usd)                                    AS cost_usd
FROM token_usage
WHERE estimated = 0
  AND COALESCE(collab_session_id, task_tag) IS NOT NULL
GROUP BY task_key, collab_phase
ORDER BY task_key, collab_phase;
```

### 10.2 Measured-vs-estimated split per task

```sql
SELECT
  COALESCE(collab_session_id, task_tag) AS task_key,
  estimated,
  SUM(input_tokens + output_tokens
      + cache_creation_input_tokens
      + cache_read_input_tokens)        AS tokens
FROM token_usage
WHERE COALESCE(collab_session_id, task_tag) IS NOT NULL
GROUP BY task_key, estimated;
```

### 10.3 Iteration counts & outcome per task

```sql
SELECT task_tag, collab_session_id, outcome,
       review_rounds, fix_commits, handoffs,
       started_at, done_at, pr_url
FROM task_outcomes
ORDER BY started_at;
```

### 10.4 Headline tokens-to-done (completed tasks only)

```sql
SELECT
  t.task_tag,
  SUM(u.input_tokens + u.output_tokens
      + u.cache_creation_input_tokens
      + u.cache_read_input_tokens)      AS tokens_to_done,
  SUM(u.cost_usd)                       AS cost_usd
FROM task_outcomes t
JOIN token_usage  u
  ON u.task_tag = t.task_tag
  OR u.collab_session_id = t.collab_session_id
WHERE t.outcome = 'merged'
  AND u.estimated = 0
GROUP BY t.task_tag;
```

Non-completions are reported by the same shape with `t.outcome IN ('failed','abandoned')`,
in a separate section (§2.2, §9.4).

> JOIN-key invariant: the `OR` join is safe only because `task_tag` and `collab_session_id`
> are each unique per task in `task_outcomes`, and a token_usage row's two keys must be
> mutually consistent (a row's `task_tag` and `collab_session_id`, when both present, refer
> to the same task). PR 5 enforces this when populating rows under an active collab session.

---

## 11. A/B protocol vs superpowers-alone

### 11.1 Corpus

8–12 real, repo-scoped tasks of comparable size (each a single PR-sized unit of work). Tasks
are drawn from genuine backlog items, not synthetic puzzles, so that "done" means the same
thing it means in production (merged + CI green). The corpus is frozen before any arm runs.

### 11.2 Arms

| Arm            | Setup                                                              |
|----------------|-------------------------------------------------------------------|
| `ironmem`      | ironmem installed + `/collab` planning discipline + worker dispatch. |
| `superpowers`  | superpowers skills alone, no ironmem server-side state, no `/collab`. |

Each task is run under both arms (or randomized assignment with enough tasks per arm — the
runner spec in PR 19 fixes the assignment; this spec fixes the *measurement*).

### 11.3 Sample size & reporting

- Minimum **8** completed tasks per arm before any headline delta is reported.
- Deltas are reported **confidence-qualified** (CI or explicit n + spread), never as a bare
  point estimate.
- The three reported deltas:
  1. **tokens-to-done per task** (§2, measured rows only),
  2. **rework loops** = `review_rounds + fix_commits` per task,
  3. **merged-rate** = fraction of attempted tasks reaching `outcome = merged`.

### 11.4 Rework-loop definition

A **rework loop** is one review-driven change cycle. Operationally, per task:

```
rework_loops(task) = task_outcomes.review_rounds + task_outcomes.fix_commits
```

The thesis predicts the `ironmem` arm has **lower rework_loops and lower tokens-to-done at
equal-or-higher merged-rate**. If it does not — at the stated confidence — the thesis is
falsified for this corpus, and that result is reported as written (§1.1).

### 11.5 Baseline gate

The Phase 2 `ironmem report` (PR 6) is the baseline-recording instrument. Phase 6 LLM-call
reductions are **gated on ≥10 tasks of baseline data** in that report before any reduction is
claimed to preserve quality.

---

## 12. Amendments (append-only)

> Every post-freeze change to any section above is recorded here as a dated, justified entry.
> The body sections are not edited silently; an amendment states what changed and why.

### 2026-06-11 — PR 04 implementation clarifications (no behavioral change to §1–§11 contracts)

Recorded while wiring the first two capture points (issue #82). These document
implementation details that §5/§8 left unspecified; no counter name, unit, source,
destination, or reporting query changed.

1. **`IRONMEM_METRICS` accepted disable values.** §8.3 says "when disabled, no rows
   are written"; the implementation disables on `0`, `false`, `no`, or `off`. Any
   other value (including `1`) leaves metrics enabled.
2. **New override seams `IRONMEM_SESSION_ID` and `IRONMEM_HARNESS`.** §5.1/§5.3 source
   `session_id`/`harness` from "hook / MCP context". Because the MCP `serve` process
   receives no harness session id in `initialize` (only `clientInfo`), two optional
   env seams pin those values when negotiation can't supply them: `IRONMEM_SESSION_ID`
   (harness session id for `session_summary` co-keying) and `IRONMEM_HARNESS`
   (`claude`|`codex` attribution; otherwise learned from `initialize.clientInfo.name`).
   Both are primarily testing seams; absent them, behavior is unchanged.
3. **`session_summary` accumulation is atomic engine-side, not read-modify-write.**
   §5.3 specifies the columns, not the write mechanism. Because the MCP-server and hook
   processes both co-key the same row (§5.3), accumulation uses a single
   `INSERT … ON CONFLICT DO UPDATE SET col = col + excluded.col` statement (max for
   `peak_occupancy_pct`, COALESCE set-once for `started_at`) so concurrent cross-process
   writes cannot lose an increment. This strengthens the §5.3 contract; it does not
   change it.
4. **`session_id` is bounded to 128 chars** by `sanitize_session_id` before becoming a
   row key, preventing an unbounded attacker-supplied key from MCP `initialize`.

### 2026-06-12 — PR 05 / issue #83: outcome-attestation semantics and deferred counters

- **2026-06-12 (PR 05 / issue #83):** `task_outcomes.outcome='merged'` is written by
  `collab_end` from `CodingComplete` as an **operator attestation** — the operator ends
  the session after the PR is merged. `final_review`/`CodingComplete` itself records only
  `done_at` + `pr_url` and leaves `outcome` NULL ("in flight"), so an unmerged PR is never
  silently counted as done (§2.2). PR 84's reporting may cross-check attestations against
  GitHub. `fix_commits` and `handoffs` remain 0 in this phase: `handoffs` awaits the
  session-handoff machinery (PR 13/15); `fix_commits` needs commit-counting the MCP server
  cannot do without git access — deferred, tracked on the roadmap.

- **Process-attribution constraint (clarification of §2.3 / §3).** Only one active collab
  session may be bound to the process attribution slot of a given MCP server process at a
  time — the constraint applies across all repos, not just the same repo+branch. The collab
  handlers (`collab_start`, `collab_start_code_review`, `collab_send`, `collab_recv`,
  `collab_wait_my_turn`) reject any attempt to bind a second still-live session to the
  process slot. Stale or ended sessions self-clear automatically. Parallel collab sessions
  require separate server processes so that `search`, pref-extract, and rerank token-usage
  rows cannot be stamped onto the wrong session.

- **§4 `review_rounds` increment semantics (clarification of §4 "each entry into
  review_local / final_review following impl/rework" wording).** As shipped, `review_rounds`
  increments exactly on phase transitions whose *new* phase buckets to `review` from a phase
  bucketing to `impl` or `rework`. In the current v3 phase order the only such edge is
  `CodeReviewFixGlobalPending → CodeReviewLocalPending` (rework → review). The
  `CodeReviewLocalPending → CodeReviewFinalPending` transition (review → review) does *not*
  increment, nor does the `CodeReviewFinalPending → CodingComplete` transition (review →
  other). This narrows §4's looser description; no counter name, unit, or destination
  changed.

### 2026-06-13 — PR 06 / issue #84: `ironmem report` cost rendering (no contract change to §1–§11 token semantics)

- **Cost rendering.** `ironmem report` (§10) renders the per-task/per-phase and
  headline **cost** as a **§7-derived** figure — `Σ tokens_of_kind × rate(model, kind)`
  using the §7.1 table embedded as a const (`crates/ironmem/src/report/cost.rs`,
  unit-pinned to §7.1) — rather than `SUM(token_usage.cost_usd)`. Reason: the
  Phase-1 capture pipeline (#81) populates `cost_usd` only for the Claude-CLI
  backend (`total_cost_usd`); summing the stored column alone under-reports cost
  for API-backed, MCP, and rerank rows. The literal §10.1/§10.4 `SUM(cost_usd)` is
  still computed and surfaced **separately** as `provider_reported_cost_usd`
  (NULL-preserving: SUM-of-all-NULL stays NULL, distinct from `0.0`). **No counter
  name, unit, source, destination, or token-aggregation query changed** — §10 token
  sums are verbatim; only the cost *rendering* is specified here.
- **Codex / unpriced rows (§7.2/§7.3).** `harness == "codex"` rows are counted in
  tokens but priced `None` (Codex cost is outside this table) even when the model id
  matches an Anthropic-priced id; unknown/unpinned models are likewise `None` and
  surfaced in `unpriced_models`. Cost is never silently `0`.
- **`--since` windowing.** `--since` accepts an RFC3339 instant or `YYYY-MM-DD`
  date, normalizes the report echo to UTC, and compares timestamps as parsed
  instants (not raw text, so `Z` and `+00:00` spellings are equivalent).
  Token-row queries (§10.1/§10.2/§10.4) filter on `ts`; the outcomes query
  (§10.3) filters on `started_at`; the headline JOIN applies `--since` to the
  token side (`ts`) only.

### 2026-06-14 — issue #113: capture-firing fix (no contract change to §1–§11 semantics)

- **Occupancy sampling is decoupled from MCP access mode (§8.2/§8.3).** §8.3 lists
  `IRONMEM_METRICS` as the *only* gate on occupancy sampling; the implementation
  additionally (and undocumentedly) required `mcp_access_mode == Trusted`
  (`allows_writes`). The lifecycle hook commands in `~/.claude/settings.json` invoke
  `ironmem hook …` without `IRONMEM_MCP_MODE`, so they ran ReadOnly and banked **zero**
  `occupancy_samples` rows. Occupancy now fires whenever `metrics_enabled()` is true,
  regardless of access mode, at **all three** sampling sites — `precompact`, `stop`,
  and `user-prompt-submit` (the UPS site is additionally budget-gated on remaining
  hook headroom, but no longer on `allows_writes`) — bringing the code in line with
  §8.3. This is safe: `occupancy_samples`/`session_summary` carry only token
  counts and occupancy %, no memory content; the SQLite connection always opens
  `READ_WRITE` (access mode is a pure application-level gate); and the **content**-write
  paths (bootstrap/mining/diary) remain gated on `allows_writes`. No counter name,
  unit, source, or aggregation changed.
- **`llm_rerank` / `pref_extract` measured rows stay OFF (decision, not contract).**
  Banking measured (`estimated=0`) `token_usage` rows requires enabling
  `IRONMEM_RERANK=llm_haiku` and/or `IRONMEM_PREF_ENRICH=1`/`IRONMEM_PREF_EXTRACTOR=llm`,
  each of which adds a Haiku LLM call to **every** search / `add_drawer`. These remain
  default-off in the dogfooding MCP env; measured baseline rows are sourced from the
  controlled, cost-gated A/B runs in issue #97 rather than accrued passively. The §11.5
  baseline gate (`≥10 measured tasks`) is therefore fed by #97, not by routine usage.

### 2026-06-15 — issue #94: v11 exploration-token attribution (additive; no change to §1–§11 token semantics)

**Schema changes (migration 011):**
- `token_usage` gains three new nullable columns: `map_status TEXT CHECK (map_status IS NULL OR map_status IN ('map_hit','map_miss'))`, `turn_id TEXT`, `area TEXT`.
- New `code_maps` table: `(repo, area) PRIMARY KEY`, `drawer_id FK→drawers`, `head_sha`, `source_files` (JSON), `built_by`, `built_at`.

**Exploration attribution (Phase 5):**
- Code-map MCP calls (`code_map_write`, `code_map_load`) emit a `source='mcp_response'` token_usage row with `map_status`, `turn_id`, `area` set. The `source` CHECK is NOT widened: `'mcp_response'` was already an allowed value established by migration 008 (`token_usage.source` enum), so no new source value is introduced. `code_map_status` is a metadata-only pre-flight and emits no attribution row.
- `map_status='map_hit'`: a `code_map_load` returned a found map with verdict `fresh`. `map_status='map_miss'`: every other case — a `stale` load, a `rescout_required`/absent load, and every `code_map_write` (cold scout / refresh).
- v0 exploration cost proxy: the code-map MCP call's response-size estimate (chars/4), recorded on the tagged live `mcp_response` row as `output_tokens`. A future phase may replace this proxy with real LLM token costs.

**Phase-5 report (§10 extension):**
- `report_exploration_delta()` aggregates all `mcp_response` rows with non-NULL `turn_id` and `map_status IN ('map_hit','map_miss')`, one unit per distinct `turn_id`. Rows with NULL `turn_id` are excluded (cannot be attributed to a turn). Each turn gets a single verdict: `map_hit` only when the turn has a hit and no miss, else `map_miss`.
- Returns `ExplorationReport { total_turns, map_hit_turns, map_miss_turns, hit_rate, mean_tokens_map_hit, mean_tokens_map_miss }`.
- Gate: the run must contain at least one `map_hit` turn and one `map_miss` turn for the delta to be meaningful (a per-run heuristic, not a per-task requirement).

### 2026-06-16 — issue #122 (PR 21): abeval live executor + "done" proxy (clarifies §11.2/§2.2; no token-semantics change)

Implements the live execution path the abeval harness (#97/#121) intentionally
shipped inert. No counter name, unit, source, or aggregation in §1–§11 changed;
this records how the §11 A/B protocol is operationalized by the runner.

1. **"done" proxy for abeval evidence (clarifies §2.2 merged + CI-green).** The
   corpus re-solves already-merged backlog items, so literal merge-to-`main` for
   every arm is impractical and would pollute history. For abeval live evidence,
   a row is `outcome:"merged", ci_green:true` **iff** (a) the arm's agent process
   completed without error (CLI envelope `is_error:false` and zero exit) **AND**
   (b) the task's frozen `gates` (`cargo test --workspace` + `cargo clippy …
   -D warnings`) pass in the produced workspace. Both are **measured** facts
   (process + gate exit codes), never a self-assertion by the agent under test;
   live rows are always `estimated:false`. This proxy is reported openly in the
   §1.1 written results — "done" for abeval means *agent-completed + gates-green
   in an isolated workspace*, distinct from production `collab_end` operator
   attestation (§12 2026-06-12). A failed agent or a red gate is not
   headline-eligible.

2. **Token capture (clarifies §2.1 source for the A/B run).** `tokens_to_done`
   (the four §2.1 components) is read from the driving `claude` CLI's
   `--output-format json` `usage` block, per arm. ironmem-arm internal Haiku
   `llm_rerank`/`pref_extract` rows remain separately captured in `token_usage`
   and are what the §11.5 baseline gate / issues #95/#96 consume.

3. **Arm isolation.** Each task×arm runs in its own workspace
   (`<out>/workspaces/<task_id>/<arm>`); `main` is never mutated by a run.

4. **Approval unchanged.** A real `claude` spawn is still reached only with BOTH
   `--execute-live` AND the cost-approval opt-in; the not-approved path errors
   before constructing or spawning anything. No paid/real run is performed by the
   #122 PR itself — the executor is verified entirely with injected fakes and
   harmless coreutils (`printf`/`true`/`false`).

5. **Deferred (carried forward).** `review_rounds`/`fix_commits` (§11.4
   rework_loops) are written as `0` by the live writer in this PR; precise
   derivation (ironmem `task_outcomes` for the ironmem arm, git history for the
   superpowers arm) is a follow-up before the headline rework delta is claimed.

### 2026-06-17 — issue #98/#97: headless collab driver + Codex token attribution (clarifies §2.1/§7.2/§11.2/§11.4)

Operationalizes the §11 A/B collab arm so it runs a real `/collab` flow and its
Codex cost is counted. No §1–§11 counter name, unit, or aggregation is removed;
these record how the arm is executed and measured.

1. **Codex tokens count toward `tokens_to_done` (clarifies §2.1).** Codex tokens
   attributed to a collab task (from `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`,
   matched by `session_meta.cwd == <task worktree>` AND a time window, taking each
   session's FINAL `token_count` cumulative `total_token_usage`) ARE included in
   that task's `tokens_to_done`: `tokens_to_done = claude.total + codex.total`.
   Codex `cost_usd` stays UNPRICED (§7.2 unchanged).

2. **Codex field mapping avoids double-counting (clarifies §2.1).** Codex's
   `total_token_usage.input_tokens` INCLUDES `cached_input_tokens` (unlike the
   Anthropic convention, where `input_tokens` excludes cache reads). To make the
   four-component §2.1 sum equal Codex's own `total_tokens`, the mapping is
   `input = input_tokens − cached_input_tokens`, `cache_read = cached_input_tokens`,
   `output = output_tokens` (already includes `reasoning_output_tokens`),
   `cache_creation = 0`. A literal field copy would inflate Codex tokens by the
   cached amount (~85% on observed sessions); this mapping counts every real token
   exactly once. (Deviation from the design spec's literal wording, verified across
   6 real sessions.)

3. **The collab arm runs via the headless driver, not `claude -p "/collab start"`
   (clarifies §11.2).** A single `claude -p` exits before the multi-actor
   dispatcher loop can hand a turn to Codex, so it never runs Codex. The arm is
   executed by a built headless driver that polls the per-task collab session and
   spawns the matching `collab-turn-*.md` worker (`claude -p`) on Claude-owned
   turns and `codex exec -s danger-full-access -C <worktree>` (isolated
   `CODEX_HOME`) on Codex-owned turns. A completed collab run with **zero
   attributed Codex sessions is INVALID** and is excluded — never recorded as full
   burn.

4. **No-push PR proxy (clarifies §2.2/§11.2 done for the collab arm).** Reaching
   `CodingComplete` normally runs `gh pr create`; for abeval the final-review
   submit is replaced by sending `final_review` with a synthetic, un-pushed
   `pr_url` (`local://abeval/<task_id>`). Intermediate collab pushes go to a
   per-task LOCAL bare remote (nothing reaches the real origin; `main` untouched).
   "done" remains the §12 2026-06-16 proxy: agent-completed (reached
   `CodingComplete`) AND frozen gates green in the worktree.

5. **`rework_loops` for the collab arm (clarifies §11.4).** `review_rounds` is the
   session's `global_review_round`; `fix_commits` is the count of commits Codex
   adds to the worktree during `CodeReviewFixGlobalPending` turns. Both are now
   measured (superseding the #122 placeholder zeros for the ironmem arm).

### 2026-06-18 — collab planning shortcut for A/B runs (clarifies §11.2)

The collab arm now uses the production shortcut planning flow in benchmark
runs: two parallel planning drafts, Claude canonical synthesis, exactly one
Codex plan-review turn, and a single human gate on Claude's final Superpowers
task plan. The `PlanLocked` bridge mechanically parses that approved markdown
into `task_list` and rejects tasks timeboxed above 20 minutes. Phase bucket names
and the `task_list` planning attribution bucket remain unchanged.

### 2026-06-19 — Claude-side token capture switches to stream-json (supersedes the 2026-06-16 #2 source clause)

**What changed.** The Claude-side component of `tokens_to_done` (§2.1) is now read
from a `claude -p --output-format stream-json --verbose` transcript, summing each
**assistant message's** `usage` deduplicated by `message.id`, instead of the
single `--output-format json` envelope's top-level `usage` block. No counter name,
unit, or aggregation in §1–§11 changed — only the *source* of the Claude-side
four-component sum.

**Why (the bug this fixes).** The single-envelope top-level `usage` reports ONLY
the orchestrator session's tokens. Task-subagents run in **separate sessions**
whose usage is never rolled up into that block (as observed in Claude Code
stream-json output and docs, 2026-06-19). The driver parsed only that envelope,
so every subagent-heavy turn was
**undercounted** — invisibly on BOTH arms' Claude side:

- the `superpowers` arm's single `claude -p` runs `subagent-driven-development`,
  whose implement subagents are sub-sessions;
- the `ironmem` collab arm's worker turns fan out to `/ultrareview-local` +
  `/pr-review-toolkit` review subagents and implement subagents.

Codex's side was always complete (its rollout is process-wide, attributed
separately per §12 2026-06-17), so this is a Claude-only correction.

**Accounting rule (canonical).** From the stream-json transcript:
1. Each line is one JSON event. A malformed line, an empty transcript, or a
   transcript with no terminal `result` event is a **loud error** (never a silent
   zero-usage row).
2. `type=="assistant"` events contribute `message.usage` keyed by `message.id`.
   Dedup is **last-write-wins per id** (a streamed/repeated id is counted once at
   its final usage). Each subagent assistant message has its own id, so its tokens
   enter the sum here.
3. The Claude-side `usage` is the field-wise sum over all distinct ids. The
   terminal `result` event's OWN top-level `usage` is **NOT** added (it is the
   parent's roll-up; adding it would double-count the parent).
4. `result` text (the model's printed output, where collab sentinel lines live)
   and `is_error` come from the terminal `result` event.
5. **Undercount exclusion.** A collab worker turn whose transcript is unparseable
   (the loud-error case of rule 1, reached via the `worker_text_and_usage`
   fallback) records a fallback ZERO for that turn and is flagged. A run that
   reaches `CodingComplete` with **any** such flagged turn is **INVALID and
   excluded** — its Claude `tokens_to_done` is a known undercount, and a completed
   run is headline-eligible. This is a partial-loss guard: the all-zero guard
   below cannot see a non-zero total that is missing one turn's tokens.

This is implemented in `benchmarks/abeval/src/stream_usage.rs::parse_stream_json`
and consumed by both the superpowers single-`claude -p` path
(`client::LiveExecutor`) and the ironmem collab worker path
(`collab_live::worker_text_and_usage`); the rule-5 exclusion lives in
`collab_driver::run_collab_task` alongside the existing zero-Claude / zero-Codex
INVALID guards. The §11.2 zero-token loud-error guard is unchanged: it now fires
on a zero *summed* total. Because this can move the §11.3 headline materially, it
gates any fresh A/B campaign — it must land before tokens-to-done is reported.

---

### §12 Amendment — 2026-06-20: Production hook persists `source='transcript'` rows

**Summary.** The `stop` and `precompact` lifecycle hooks now persistently write
full-transcript token-usage rows to `token_usage` with `source='transcript'`,
`estimated=false`, in addition to the existing occupancy samples and
`source='mcp_response'` rows. This makes the **production ironmem arm** measurement-
valid for the abeval A/B experiment — previously, real `/collab` runs had no
`tokens_to_done` because only `mcp_response` (serving footprint) rows were written.
No counter names, units, §1–§11 aggregation rules, or schema migrations changed.

**Idempotency key.** Each row is keyed by `turn_id`:
- Claude stream-json: `transcript:<harness-session-id-or-content-hash>:<message-id>`.
  One row per distinct `message.id`.
- Codex rollout: `transcript:<harness-session-id-or-content-hash>:codex-final`.
  One cumulative row per session.

Dedup: `SELECT … WHERE source='transcript' AND turn_id=?` before INSERT — if found,
UPDATE the four components; else INSERT. Scoped to `source='transcript'` so
`mcp_response`/`llm_rerank`/`pref_extract` rows with the same `turn_id` are not
affected.

**Codex cached-token subtraction.** Codex `input_tokens` INCLUDES
`cached_input_tokens`, unlike the Anthropic convention. Production hook maps:
`input = input_tokens − cached_input_tokens`, `cache_read = cached_input_tokens`,
`cache_creation = 0`. A `cached > input` value is a loud warn (row skipped).

**Full-transcript parse (not the occupancy tail).** The occupancy tail reader
(`extract_last_assistant_usage`) reads only the last 2 MB and returns a single
last-assistant usage — undercounting subagent-heavy streams. Transcript token
persistence uses a separate full-file parser (`crates/ironmem/src/metrics/transcript.rs`)
that reads the entire transcript and emits one row per distinct `message.id` (Claude)
or one cumulative row (Codex). The tail reader is unchanged.

**Transcript-row task_tag.** For rows inside an active collab session, `task_tag`
is set to `collab_session_id`. This is transcript-row-specific and preserves the
§10.4 OR-join invariant (`u.task_tag = t.task_tag OR u.collab_session_id =
t.collab_session_id`) so transcript rows are visible in `ironmem report`.

**Gate.** Transcript persistence fires under `metrics_enabled()` (same gate as
occupancy), DECOUPLED from `allows_writes`/`mcp_access_mode` (same §113 pattern as
occupancy decoupling). `IRONMEM_METRICS=0` suppresses transcript rows too. `stop` and
`precompact` only — UserPromptSubmit remains occupancy-only (N3: no real message id
available in UPS).

**Implementation.** `crates/ironmem/src/metrics/transcript.rs` (parsers),
`crates/ironmem/src/db/metrics.rs::upsert_transcript_token_usage` (idempotent DB
helper), `crates/ironmem/src/hook.rs::persist_transcript_tokens` (wiring).
