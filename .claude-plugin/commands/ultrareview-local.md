---
description: Deep local multi-agent review — correctness + code + security + architecture + docs + comments + tests + error-handling + type-design + concurrency in parallel, adversarially verified, synthesized verdict (inline, no files, no GitHub writes)
argument-hint: [pr-number | pr-url | blank for local review]
---

# Ultra Review (Local)

Runs four core specialist agents in parallel (code-reviewer, security-reviewer, architect, doc-reviewer) — plus conditional agents when the diff matches their trigger (marketing-claims auditor, comment-analyzer, pr-test-analyzer, silent-failure-hunter, type-design-analyzer, concurrency-reviewer, performance-reviewer) — then adversarially verifies every CRITICAL/HIGH finding, synthesizes, deduplicates, and decides. Use for large diffs, security-sensitive changes, or architectural work. Stays entirely local — nothing written to disk, nothing posted to GitHub.

**Input**: $ARGUMENTS

---

## Mode

- `$ARGUMENTS` contains a PR number / URL / branch → **PR Mode**
- Empty → **Local Mode** (uncommitted + staged + untracked changes on `HEAD`)

---

## Phase 1 — FETCH

**PR Mode:**
```bash
gh pr view <N> --json number,title,body,baseRefName,headRefName,headRefOid,changedFiles,additions,deletions,isDraft,mergeStateStatus
gh pr diff <N> --name-only
```
After resolving `<repo_path>`, `<baseRefName>`, and `<headRefName>`, attempt:

```bash
ironmem review-diff --repo <repo_path> --base <baseRefName> --head <headRefName>
```

Inject its compact stdout **only on success**. On error, unavailable feature,
or a nonbeneficial artifact, discard its output and use the exact existing raw
fallback `gh pr diff <N>`. Do not retain the full raw diff when the artifact
succeeds. Diff range for agents: `<baseRefName>...<headRefName>`. If PR not
found → stop.
Record whether the local working tree is at the PR head: `git rev-parse HEAD` vs `headRefOid`. If they differ, Phase 4 validation must be `n/a (working tree ≠ PR head)` — never run tests on a different tree and present the result as the PR's.

**Local Mode:**
```bash
git status --short
git add -N . 2>/dev/null || true   # register untracked files as intent-to-add so new files appear in the diff
git diff HEAD --name-only
```
First attempt:

```bash
ironmem review-diff --repo <repo_path> --worktree
```

Inject its compact stdout **only on success**. On error, unavailable feature,
or a nonbeneficial artifact, discard its output and use the exact existing raw
fallback `git diff HEAD`. Do not retain the full raw diff when the artifact
succeeds. If empty → stop: "Nothing to review."

Record title, file list, additions/deletions, draft flag, and the selected
review input. The compact artifact's index supports exact source expansion:

```bash
# PR range form (use --worktree instead of --base/--head in Local Mode)
ironmem review-diff --repo <repo_path> --base <baseRefName> --head <headRefName> --expand-file <path> --hunk <ordinal>
```

If >50 files or >2000 additions, warn — but the parallel split scales.

---

## Phase 2 — CONTEXT (keep it thin)

Read only what the agents won't:
1. Root `CLAUDE.md` and matching `.claude/docs/` rules
2. PR body (intent, linked issues, test plan)
3. Plan artifacts under `.claude/PRPs/plans/` or `docs/` matching the branch

Do **not** pre-read the full contents of changed files — the Phase 1 review
input plus paths is enough; let the agents inspect source independently.

---

## Phase 3 — PARALLEL REVIEW

### Trigger detection (before dispatch)

Grep the selected review input (artifact normally, raw fallback only) for
conditional-agent triggers. When the artifact is insufficient to classify a
trigger, expand its indexed file/hunk or inspect that source directly; do not
retain a whole raw diff solely for trigger detection.

| Agent | Grep the diff for |
|---|---|
| comment-analyzer (F) | added/modified docstrings, `///`, `"""`, `/** … */`, or block comments beyond trivial one-liners |
| pr-test-analyzer (G) | any non-doc/config change to application logic (default ON unless diff is docs/config-only) |
| silent-failure-hunter (H) | `try`, `except`, `catch`, `unwrap_or`, `.ok()`, `.catch(`, `rescue`, `recover(`, retry/fallback logic |
| type-design-analyzer (I) | `class .*BaseModel`, `@dataclass`, `enum`, `struct`, `interface`, new domain types |
| concurrency-reviewer (J) | `async`, `await`, `spawn`, `thread`, `Mutex`, `RwLock`, `Arc<`, `atomic`, `BEGIN`/`COMMIT`/`transaction`, `UPDATE ... SET`, read-then-write on shared state, queue/channel ops |
| performance-reviewer (K) | queries inside loops, new DB queries without LIMIT/index, O(n²) scans over collections, allocation in hot loops, unbounded caches/collections |

### Dispatch

Dispatch the four core agents (A–D) in **a single message** (parallel tool calls), plus any triggered conditional agents (E–K) — all in that same message, since they're independent.

**Shared inputs for every agent**: the diff range, file list, selected review
input, context summary, and the instruction to inspect changed source and
callers independently before reading whole files. When the compact artifact is
in use, expand an indexed selection with `ironmem review-diff --repo
<repo_path> --base <baseRefName> --head <headRefName> --expand-file <path>
--hunk <ordinal>` (or the Local-Mode `--worktree` form); use a targeted `git
diff <range> -- <path>` only when needed. They must review what changed rather
than the whole codebase.

**Shared output contract (include verbatim in every brief):**
> Findings only, inline, grouped by severity (CRITICAL / HIGH / MEDIUM / LOW). Each finding: `file:line — issue — failure scenario — suggested fix`. The **failure scenario is mandatory for CRITICAL/HIGH**: state the concrete inputs or state that reach the code and the wrong behavior that results ("X called with empty list → index panic at line N"). A finding you cannot express as a failure scenario is at most MEDIUM. Report only findings you are >80% confident in; do not pad. Word budget: 600 words for diffs under ~400 changed lines, up to 1200 for larger diffs — when over budget, drop LOWs first, never CRITICALs.

**Blast-radius requirement (include in briefs A and C):**
> For each changed public/exported symbol (function signature, return semantics, enum variants, API shape), locate its callers — `grep` is fine; if the `mcp__ironmem__symbol_neighbors` / `symbol_lookup` tools are available and the repo is indexed, use them — and verify each caller still behaves correctly under the new semantics. Bugs at the boundary between changed and unchanged code count double.

### Agent A — `code-reviewer` (correctness lens)
**Brief**: You are in diff-review mode, not plan-alignment mode: skip plan comparison, skip praise. Hunt for bugs: trace data flow through every changed function; simulate execution on edge inputs (empty, None/null, zero, negative, max, unicode, concurrent); off-by-one, inverted conditions, wrong operator, missed early return; error paths that corrupt state; resource leaks. Also: type safety, dead code, magic numbers. Do **not** review security, architecture, or docs — other lenses own those.

### Agent B — `security-reviewer`
**Brief**: OWASP Top 10, injection, auth/authz, secret exposure, SSRF, path traversal, unsafe crypto, input validation at boundaries, rate limiting, error-message leakage, deserialization. Use ecosystem-appropriate scanners when present (`cargo audit`, `pip-audit`, `bandit`, `npm audit`, `gitleaks`). **Read-only: findings only, never edit files.** Do not review general code quality — Agent A owns it.

### Agent C — `architect`
**Brief**: You are reviewing a diff, not designing a system: no ADRs, no scalability roadmaps. Focus on defects with architectural cause: state-machine correctness (unreachable/missing transitions), migration safety (data loss, non-reversible steps), API contract stability (breaking change without versioning), coupling that will force shotgun surgery, abstraction placed in the wrong layer, invariants held in one module silently assumed by another. Run `git diff <range>` first.

### Agent D — `doc-reviewer`
Documentation completeness *for this diff*: missing public-API docstrings, breaking changes without CHANGELOG/migration notes, new env vars / config flags absent from `.env.example` or README, stale comments referring to removed/renamed code, README examples that drift from new behavior, codemap entries missing for new modules. Findings only — never edits.

### Agent E — marketing-claims auditor (conditional)
Dispatch **only** when the project defines `.claude/agents/marketing-copy-auditor.md` (use that agent type) or names its claim surfaces in `CLAUDE.md`/`.claude/docs/` — and the diff touches those surfaces (marketing copy, pricing/plan config, feature flags, coverage/count claims, JSON-LD). No project-defined claim surfaces → skip; do not guess paths.

**Brief**: cross-check every user-visible claim against its ground-truth source in code/config. Each finding: claim → ground-truth source → verdict → suggested fix. Read-only.

### Agent F — `pr-review-toolkit:comment-analyzer` (conditional)
**Brief**: comment accuracy vs the code it describes, comment rot (comment says X, code does Y), missing context for non-obvious logic. Findings only. Ignore project-specific conventions baked into your agent definition that this repo doesn't use.

### Agent G — `pr-review-toolkit:pr-test-analyzer` (conditional)
**Brief**: behavioral test coverage gaps, missing edge cases (happy path only, no negative cases), tests that assert on mocks instead of real behavior, tests that would still pass if the new logic were deleted. Findings only.

### Agent H — `pr-review-toolkit:silent-failure-hunter` (conditional)
**Brief**: swallowed exceptions, bare `except:` / empty catch blocks / discarded fallible results (`let _ =` on a `Result`), fallback behavior that masks real failures instead of surfacing them, missing error logging, retries that exhaust silently. Findings only. Ignore project-specific logging functions or error-ID registries named in your agent definition unless this repo actually has them.

### Agent I — `pr-review-toolkit:type-design-analyzer` (conditional)
**Brief**: encapsulation, invariant expression (can the type be constructed in an invalid state?), whether the type earns its complexity. Findings only.

### Agent J — concurrency reviewer (conditional, `general-purpose`)
**Brief**: data races and TOCTOU; read-modify-write on shared state without a lock or atomic `UPDATE ... WHERE ... RETURNING`; missing or wrongly-scoped transactions; lock-ordering deadlocks; non-idempotent operations that get retried (webhooks, queue consumers); await points while holding locks; channel/queue operations that can drop or duplicate messages. Every CRITICAL/HIGH must spell out the interleaving ("A reads balance, B commits, A writes stale"). Findings only, same output contract.

### Agent K — performance reviewer (conditional, `performance-optimizer`)
**Brief**: N+1 queries, unbounded queries/collections, missing pagination, O(n²) on user-scaled data, allocation or I/O in hot loops, missing/wrong indexes for new query shapes. Findings only — **read-only, never edit files.** Skip micro-optimizations; flag only what degrades at realistic scale.

**All dispatched agents run concurrently.** Do not proceed until all return.

If one returns empty, note it in Lens Coverage — do not retry.

---

## Phase 4 — VALIDATE

**PR Mode guard**: only run validation if the working tree is at the PR head (checked in Phase 1). Otherwise record every check as `n/a (working tree ≠ PR head)` — a green run on the wrong tree is worse than no run.

Detect project type, run only what applies. Record pass/fail per check. Missing scripts = `n/a`, not a failure.

- **Rust** (`Cargo.toml`): `cargo fmt --all -- --check` · `cargo clippy --workspace --all-targets --all-features -- -D warnings` · `cargo test --workspace`
- **Node/TS** (`package.json`): `npm run typecheck || npx tsc --noEmit` · `npm run lint` · `npm test`
- **Go** (`go.mod`): `go vet ./...` · `go test ./...` · `go build ./...`
- **Python** (`pyproject.toml`): `ruff check .` · `pytest`

---

## Phase 5 — SYNTHESIZE

Merge all dispatched lenses:

1. **Dedup** — same file:line from two agents → keep the clearer wording, tag with both lenses (e.g. `[code+security]`)
2. **Escalate** — two agents flag same issue at different severities → take the higher
3. **Promote root causes** — 2+ findings share a cause → surface it as one HIGH instead of N MEDIUMs
4. **Demote the unfalsifiable** — a CRITICAL/HIGH with no concrete failure scenario drops to MEDIUM
5. **Drop noise** — style nits already covered by a passing linter

---

## Phase 5.5 — VERIFY (adversarial)

Every CRITICAL and HIGH that survives Phase 5 gets independently verified before it can drive the decision.

- Dispatch one `general-purpose` verifier per finding, **all in a single parallel message**, cap 8 (prioritize CRITICALs, then HIGHs by impact; anything past the cap stays tagged `UNVERIFIED`).
- **Verifier brief**: "Adversarially verify this review finding — your job is to REFUTE it. Finding: `<file:line — issue — failure scenario>`. Read the actual code, trace the claimed path, check the claimed inputs can actually reach it. Verdict: `CONFIRMED` (quote the code path that proves it), `REFUTED` (quote the guard/invariant that prevents it), or `PLAUSIBLE` (could not prove either way). One paragraph max."
- `REFUTED` → drop from findings; list under "Refuted during verification" in the report so the signal isn't silently lost.
- `CONFIRMED` / `PLAUSIBLE` / `UNVERIFIED` → keep, tagged.

This pass exists so the finder agents can afford to be aggressive: report the suspicious thing, let the verifier kill the false positives.

---

## Phase 6 — DECIDE

Decisions count only findings that survived Phase 5.5 (CONFIRMED, PLAUSIBLE, or UNVERIFIED — refuted ones are gone).

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
CRITICAL: <file:line — issue — failure scenario — fix [tags] [CONFIRMED|PLAUSIBLE|UNVERIFIED]>
HIGH: ...
MEDIUM: ...
LOW: ...

## Refuted during verification
<finding → refuting evidence, or "None">

## Cross-cutting patterns
<root causes from multiple findings, or "None">

## Validation
<check → pass / fail / n/a (+ reason when n/a)>

## Lens coverage
- code-reviewer (correctness): <N findings>
- security-reviewer: <N findings>
- architect: <N findings>
- doc-reviewer: <N findings>
- marketing-claims auditor: <N findings | skipped (no project claim surfaces)>
- comment-analyzer: <N findings | skipped (no substantial comments)>
- pr-test-analyzer: <N findings | skipped (doc/config-only diff)>
- silent-failure-hunter: <N findings | skipped (no error-handling changes)>
- type-design-analyzer: <N findings | skipped (no new types)>
- concurrency-reviewer: <N findings | skipped (no concurrency surface)>
- performance-reviewer: <N findings | skipped (no perf-sensitive changes)>
- verification: <N confirmed / N plausible / N refuted / N unverified>
```

End with one line: the next step that fits the decision.

---

## Edge cases

- **No `gh` CLI** → PR mode falls back to `git diff origin/<base>...HEAD`. Warn.
- **Docs/config-only diff** → skip security agent and pr-test-analyzer; run code + architect + doc-reviewer (doc-reviewer still useful — catches stale cross-references between docs).
- **Diff <50 lines** → suggest `/code-review` instead; proceed only if user confirmed.
- **Agent returns empty** → note in Lens Coverage, no retry.
- **Validation script missing** → record as `n/a`, don't block.
- **Working tree not at PR head** → validation `n/a` with reason; agents still review via the diff range (they read the base/head refs, not the working tree).
- **Merge conflicts in PR** → surface under Validation as a HARD fail; still run the four core agents.
- **`pr-review-toolkit` plugin not installed/enabled** → conditional agents F–I fall back to a general-purpose agent dispatched with the same brief (`agentType: general-purpose` instead of the plugin-specific type), so the review degrades gracefully instead of erroring. Same fallback if `performance-optimizer` is unavailable for Agent K.
- **Verification budget** → if more than 8 CRITICAL/HIGH survive Phase 5, that is itself a signal — say so in the summary; verify the top 8 and tag the rest `UNVERIFIED`.
