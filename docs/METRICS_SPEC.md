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
