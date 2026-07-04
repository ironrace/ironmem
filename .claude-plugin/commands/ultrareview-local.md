---
description: Deep local multi-agent review — code + security + architecture + docs + comments + tests + error-handling + type-design in parallel, synthesized verdict (inline, no files, no GitHub writes)
argument-hint: [pr-number | pr-url | blank for local review]
---

# Ultra Review (Local)

Runs four core specialist agents in parallel (code-reviewer, security-reviewer, architect, doc-reviewer) — plus up to five conditional agents when the diff matches their trigger (marketing-claims auditor, comment-analyzer, pr-test-analyzer, silent-failure-hunter, type-design-analyzer, the last four borrowed from the `pr-review-toolkit` plugin) — then synthesizes, deduplicates, and decides. Use for large diffs, security-sensitive changes, or architectural work. Stays entirely local — nothing written to disk, nothing posted to GitHub.

**Input**: $ARGUMENTS

---

## Mode

- `$ARGUMENTS` contains a PR number / URL / branch → **PR Mode**
- Empty → **Local Mode** (uncommitted + staged changes on `HEAD`)

---

## Phase 1 — FETCH

**PR Mode:**
```bash
gh pr view <N> --json number,title,body,baseRefName,headRefName,changedFiles,additions,deletions,isDraft,mergeStateStatus
gh pr diff <N> --name-only
```
Diff range for agents: `<baseRefName>...<headRefName>`. If PR not found → stop.

**Local Mode:**
```bash
git status --short
git diff HEAD --name-only
```
If empty → stop: "Nothing to review."

Record title, file list, additions/deletions, draft flag. If >50 files or >2000 additions, warn — but the parallel split scales.

---

## Phase 2 — CONTEXT (keep it thin)

Read only what the agents won't:
1. Root `CLAUDE.md` and matching `.claude/docs/` rules
2. PR body (intent, linked issues, test plan)
3. Plan artifacts under `.claude/PRPs/plans/` or `docs/` matching the branch

Do **not** pre-read changed files. Pass paths to the agents and let them read.

---

## Phase 3 — PARALLEL REVIEW

Dispatch the four core agents (A-D) in **a single message** (parallel tool calls), plus any of the conditional agents (E-I) whose trigger matches the diff — all in that same message, since they're independent. Shared inputs: PR number or diff range, file list, context summary. Shared output contract: inline findings only, grouped by severity (CRITICAL / HIGH / MEDIUM / LOW), each with `file:line — issue — suggested fix`, under 600 words.

### Agent A — `code-reviewer`
Correctness, type safety, pattern compliance, error handling, test coverage gaps, dead code, function/file size, magic numbers, naming.

### Agent B — `security-reviewer`
OWASP Top 10, injection, auth/authz, secret exposure, SSRF, path traversal, unsafe crypto, input validation at boundaries, rate limiting, error-message leakage, deserialization.

### Agent C — `architect`
System design, coupling/cohesion, state-machine correctness, migration safety, API contract stability, scalability, abstraction placement, whether the change belongs where it landed.

### Agent D — `doc-reviewer`
Documentation completeness *for this diff*: missing public-API docstrings, breaking changes without CHANGELOG/migration notes, new env vars / config flags absent from `.env.example` or README, stale comments referring to removed/renamed code, README examples that drift from new behavior, codemap entries missing for new modules. Findings only — never edits.

### Agent E — marketing-claims auditor (conditional)
Dispatch **only** when the diff touches claim-bearing surfaces. If the project defines `.claude/agents/marketing-copy-auditor.md`, use that agent type; otherwise dispatch a general agent with the same brief.

**Trigger paths** (any match → dispatch):
- `backend/app/ingestion/*` (scrapers added/activated/removed)
- migrations that add or activate counties
- `backend/app/services/billing_service.py` (`PLAN_CONFIG`, `OVERAGE_PRICES`, `CREDIT_PACKS`)
- `backend/app/core/config.py` feature flags (e.g. `CONTRACTS_ENABLED`)
- `frontend/src/lib/marketing-copy.ts`, `frontend/index.html`
- `frontend/src/pages/marketing/*`, `frontend/src/hooks/use-public-coverage.ts`

**Brief**: cross-check every user-visible claim in marketing surfaces against ground truth — coverage numbers vs `GET /api/v1/public/coverage` (or the County model / migrations), advertised plan features vs `PLAN_FEATURES` + feature flags (gated-OFF features must not be advertised), cadence claims vs `workers/scheduled.py` schedules, prices vs `PLAN_CONFIG`, JSON-LD in `index.html` vs all of the above. Each finding: claim → ground-truth source → verdict → suggested fix. Read-only.

### Agent F — `pr-review-toolkit:comment-analyzer` (conditional)
**Trigger**: diff adds or modifies docstrings/comments beyond trivial one-liners, or touches a public API surface where stale comments are likely.
**Brief**: comment accuracy vs the code it describes, comment rot (comment says X, code does Y), missing context for non-obvious logic. Findings only.

### Agent G — `pr-review-toolkit:pr-test-analyzer` (conditional)
**Trigger**: diff is not doc/config-only — i.e. it changes application logic, endpoints, or behavior (this project requires tests for all new endpoints per `CLAUDE.md`).
**Brief**: behavioral test coverage gaps, missing edge cases (happy path only, no negative cases), tests that assert on mocks instead of real behavior. Findings only.

### Agent H — `pr-review-toolkit:silent-failure-hunter` (conditional)
**Trigger**: diff touches `try`/`except`/`catch`, error handling, retries, or fallback logic.
**Brief**: swallowed exceptions, bare `except:` / empty catch blocks, fallback behavior that masks real failures instead of surfacing them, missing error logging. Findings only.

### Agent I — `pr-review-toolkit:type-design-analyzer` (conditional)
**Trigger**: diff adds or modifies a Pydantic model, dataclass, enum, or other new type.
**Brief**: encapsulation, invariant expression (can the type be constructed in an invalid state?), whether the type earns its complexity. Findings only.

**All dispatched agents run concurrently.** Do not proceed until all return.

If one returns empty, note it in Lens Coverage — do not retry.

---

## Phase 4 — VALIDATE

Detect project type, run only what applies. Record pass/fail per check. Missing scripts = `n/a`, not a failure.

- **Rust** (`Cargo.toml`): `cargo fmt --all -- --check` · `cargo clippy --workspace --all-targets --all-features -- -D warnings` · `cargo test --workspace`
- **Node/TS** (`package.json`): `npm run typecheck || npx tsc --noEmit` · `npm run lint` · `npm test`
- **Go** (`go.mod`): `go vet ./...` · `go test ./...` · `go build ./...`
- **Python** (`pyproject.toml`): `ruff check .` · `pytest`

---

## Phase 5 — SYNTHESIZE

Merge all dispatched lenses (including Agent E when it ran — claim-drift findings dedup and escalate like any other):

1. **Dedup** — same file:line from two agents → keep the clearer wording, tag with both lenses (e.g. `[code+security]`)
2. **Escalate** — two agents flag same issue at different severities → take the higher
3. **Promote root causes** — 2+ findings share a cause → surface it as one HIGH instead of N MEDIUMs
4. **Drop noise** — style nits already covered by a passing linter

---

## Phase 6 — DECIDE

| Condition | Decision |
|---|---|
| Zero CRITICAL/HIGH, validation passes | **APPROVE** |
| Only MEDIUM/LOW, validation passes | **APPROVE with comments** |
| Any HIGH or validation failure | **REQUEST CHANGES** |
| Any CRITICAL | **BLOCK** |

Draft PRs → always **COMMENT**, regardless of findings.

---

## Phase 7 — REPORT

Output **inline only**. Do NOT write files. Do NOT post to GitHub. Do NOT write to `.claude/PRPs/reviews/`.

```
# Ultra Review (local): <PR #N | Local> — <TITLE>

Decision: APPROVE | APPROVE with comments | REQUEST CHANGES | BLOCK | COMMENT (draft)

## Summary
<2-3 sentences covering every lens that was dispatched>

## Findings
CRITICAL: <file:line — issue — fix [tags]>
HIGH: ...
MEDIUM: ...
LOW: ...

## Cross-cutting patterns
<root causes from multiple findings, or "None">

## Validation
<check → pass / fail / n/a>

## Lens coverage
- code-reviewer: <N findings>
- security-reviewer: <N findings>
- architect: <N findings>
- doc-reviewer: <N findings>
- marketing-claims auditor: <N findings | skipped (no claim surfaces)>
- comment-analyzer: <N findings | skipped (no substantial comments)>
- pr-test-analyzer: <N findings | skipped (doc/config-only diff)>
- silent-failure-hunter: <N findings | skipped (no error-handling changes)>
- type-design-analyzer: <N findings | skipped (no new types)>
```

End with one line: the next step that fits the decision.

---

## Edge cases

- **No `gh` CLI** → PR mode falls back to `git diff origin/<base>...HEAD`. Warn.
- **Docs/config-only diff** → skip security agent and pr-test-analyzer; run code + architect + doc-reviewer (doc-reviewer still useful — catches stale cross-references between docs).
- **Diff <50 lines** → suggest `/code-review` instead; proceed only if user confirmed.
- **Agent returns empty** → note in Lens Coverage, no retry.
- **Validation script missing** → record as `n/a`, don't block.
- **Merge conflicts in PR** → surface under Validation as a HARD fail; still run the four core agents.
- **`pr-review-toolkit` plugin not installed/enabled** → conditional agents F-I fall back to a general-purpose agent dispatched with the same brief (`agentType: general-purpose` instead of the plugin-specific type), so the review degrades gracefully instead of erroring.
