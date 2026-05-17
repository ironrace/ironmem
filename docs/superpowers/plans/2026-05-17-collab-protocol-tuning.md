# Collab Protocol Tuning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring `docs/COLLAB.md`, `.claude-plugin/commands/collab.md`, `.codex-plugin/prompts/collab.md` into agreement with the server's existing `MAX_REVIEW_ROUNDS = 2` enforcement, normalize timing event names + structured metadata, add a bounded polling backoff for silent Codex bg phases, and document two anti-removal guardrails (`/ultrareview-local`, SDD reviewer model-pinning).

**Architecture:** Pure docs + prompts edit. No Rust source touched. Server stays the source of truth; this round fixes drift in the human/agent-facing surface.

**Tech stack:** Markdown only. Pre-commit hook runs `cargo fmt --check` + `cargo clippy` (both trivial passes since no Rust changes).

---

## Task 1: Update docs/COLLAB.md

**Files:**
- Modify: `docs/COLLAB.md` (~60-80 lines added across 5 substantive edits)

**Acceptance:**
- `grep -c "MAX_REVIEW_ROUNDS" docs/COLLAB.md` ≥ 1
- `grep -c "phase=\|round=" docs/COLLAB.md` ≥ 1
- `grep -c "t4_phase_advanced\b" docs/COLLAB.md` ≥ 1
- `grep -c "t4_phase_advanced_to_" docs/COLLAB.md` = 0
- `grep -c "ultrareview-local" docs/COLLAB.md` ≥ 1 (with "overlap audit" nearby)
- `grep -c "subagent-driven\|SDD" docs/COLLAB.md` ≥ 1 (non-pinning note present)
- `grep -ci "uncapped\|indefinite back-and-forth" docs/COLLAB.md` = 0
- `grep -c "_round2\|_round3\|dispatched_round\|returned_round" docs/COLLAB.md` = 0

**Substantive edits (5):**
1. Add a "Review cap" subsection naming `MAX_REVIEW_ROUNDS = 2`, citing `crates/ironmem/src/collab/state_machine/mod.rs:28`, explaining force-finalize semantics.
2. Update the timing instrumentation section: event names are stable base names; phase + round detail in structured `phase=` / `round=` fields; rename `t4_phase_advanced_to_<phase>` → `t4_phase_advanced`; mark `_round<N>` suffixes legacy/incorrect.
3. Mirror the polling backoff section (curve: 10s → 20s @ 60s → 30s @ 300s cap; reset on phase/stdout/exit/error; 600s hang detection unchanged).
4. Add anti-removal guardrail for `/ultrareview-local` near `CodeReviewLocalPending` discussion.
5. Add SDD reviewer model-pinning protocol-boundary note.

**Commit:** `docs(collab): docs/COLLAB.md — review cap + structured events + backoff + guardrails`

---

## Task 2: Update .claude-plugin/commands/collab.md

**Files:**
- Modify: `.claude-plugin/commands/collab.md` (~30-40 lines)

**Acceptance:**
- `grep -c "MAX_REVIEW_ROUNDS" .claude-plugin/commands/collab.md` ≥ 1
- `grep -c "t4_phase_advanced\b" .claude-plugin/commands/collab.md` ≥ 1
- `grep -c "t4_phase_advanced_to_" .claude-plugin/commands/collab.md` = 0
- `grep -c "backoff\|escalate" .claude-plugin/commands/collab.md` ≥ 1
- `grep -c "ultrareview-local" .claude-plugin/commands/collab.md` ≥ 1
- `grep -ci "uncapped\|indefinite back-and-forth" .claude-plugin/commands/collab.md` = 0
- `grep -c "_round2\|_round3\|dispatched_round\|returned_round" .claude-plugin/commands/collab.md` = 0

**Substantive edits (4):**
1. `PlanCodexReviewPending` row in v1 planning table: add cap callout ("Codex gets at most 2 review rounds; server enforces MAX_REVIEW_ROUNDS = 2; after the 2nd review the phase advances to PlanClaudeFinalizePending regardless of verdict").
2. Timing event table: update to structured-metadata form; rename `t4_phase_advanced_to_<phase>` → `t4_phase_advanced`; mark suffixed names legacy.
3. Polling loop section: add the bounded backoff curve + reset conditions + explicit "does NOT change Plan Mode idle gaps" note.
4. `CodeReviewLocalPending` row: add the `/ultrareview-local` anti-removal guardrail note inline or as a callout below the table.

**Commit:** `docs(collab): .claude-plugin/commands/collab.md — review cap + structured events + backoff + guardrail`

---

## Task 3: Update .codex-plugin/prompts/collab.md

**Files:**
- Modify: `.codex-plugin/prompts/collab.md` (~10-15 lines)

**Acceptance:**
- `grep -c "review round" .codex-plugin/prompts/collab.md` ≥ 1 (cap language for Codex)
- `grep -ci "uncapped\|indefinite back-and-forth" .codex-plugin/prompts/collab.md` = 0
- `grep -c "_round2\|_round3\|dispatched_round\|returned_round" .codex-plugin/prompts/collab.md` = 0
- If the file mirrors the event list: `grep -c "t4_phase_advanced_to_" .codex-plugin/prompts/collab.md` = 0

**Substantive edits (1-2):**
1. Mirror review cap language Codex-facing: "you have at most 2 reviews; on the 2nd round the server force-finalizes whether you `approve` or `request_changes`".
2. If the file carries the event list, update to structured-metadata form. (Verify before editing — file may not duplicate the event list.)

**Commit:** `docs(collab): .codex-plugin/prompts/collab.md — Codex-facing review cap + event normalization mirror`

---

## Task 4: Verify .codex-plugin/prompts/collab-batch-impl.md

**Files:**
- Inspect: `.codex-plugin/prompts/collab-batch-impl.md`
- Modify: only if it duplicates affected event-list or backoff language

**Acceptance:**
- If no relevant content present → no commit; document as "verified, no changes needed" in PR description.
- If relevant content present: same grep gates as Task 3 apply; commit message: `docs(collab): collab-batch-impl.md — event normalization mirror`.

---

## Task 5: Final gates + acceptance verification

**Files:**
- No file edits in this task. Verification only.

**Acceptance (run all 14 gates from the v1 plan):**

```bash
# Review cap
test "$(grep -c 'MAX_REVIEW_ROUNDS' docs/COLLAB.md)" -ge 1
test "$(grep -c 'MAX_REVIEW_ROUNDS' .claude-plugin/commands/collab.md)" -ge 1
test "$(grep -c 'review round' .codex-plugin/prompts/collab.md)" -ge 1

# No uncapped language
! grep -qi 'uncapped\|indefinite back-and-forth' docs/COLLAB.md .claude-plugin/commands/collab.md .codex-plugin/prompts/collab.md

# No legacy event suffixes
! grep -q '_round2\|_round3\|dispatched_round\|returned_round' docs/COLLAB.md .claude-plugin/commands/collab.md .codex-plugin/prompts/collab.md

# Structured metadata
grep -q 'phase=\|round=' docs/COLLAB.md
grep -q 't4_phase_advanced\b' docs/COLLAB.md
grep -q 't4_phase_advanced\b' .claude-plugin/commands/collab.md
! grep -q 't4_phase_advanced_to_' docs/COLLAB.md
! grep -q 't4_phase_advanced_to_' .claude-plugin/commands/collab.md

# Backoff documented
grep -q 'backoff\|escalate' .claude-plugin/commands/collab.md

# Anti-removal guardrail
grep -q 'ultrareview-local' docs/COLLAB.md   # manual check: "overlap audit" nearby

# SDD note
grep -q 'subagent-driven\|SDD' docs/COLLAB.md   # manual check: non-pinning note

# Rust gates (trivial — no Rust changes)
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --release -- -D warnings
cargo test --workspace --release
```

If any gate fails, fix in the corresponding earlier task before sending `implementation_done`.

**No commit in this task** (verification only).

---

## Verification

End-to-end: re-read each file's diff and confirm the substantive edits match the v1 plan. The Rust gates are passing-by-construction since no Rust changes; main risk is markdown grep gates missing because of accidental rephrasing.

After Task 5: `implementation_done` send goes to the collab session. PR creation happens in `CodeReviewFinalPending` (separate phase, owned by Claude).
