# abeval — A/B Eval Harness (METRICS_SPEC §11)

`benchmarks/abeval` is an excluded sibling crate of `benchmarks/provbench/baseline`.
It freezes an 8–12 task corpus of real repo-scoped backlog work and can run/record both
METRICS_SPEC §11 arms end-to-end. This PR delivers harness + corpus + runner + tests only.
**No paid runs are shipped here.**

---

## Corpus format

File: `corpus/tasks.jsonl` (one JSON object per line, diff-friendly).

Each task object has:

| Field | Type | Description |
|-------|------|-------------|
| `id` | string | Unique stable basename-safe slug, e.g. `abeval-01-slug` |
| `title` | string | Human-readable title |
| `source` | string | Real reference: `issue:#NN`, `pr:#NN`, or `backlog:<ref>` |
| `repo_scope` | string[] | Path globs for the relevant area |
| `prompt` | string | Task instruction given to the arm |
| `acceptance` | string[] | ≥1 acceptance criteria |
| `gates` | string[] | ≥1 gate commands (e.g. `cargo test --workspace`) |
| `setup_notes` | string? | Optional setup context |

**Invariants** enforced by `abeval validate`:
1. 8 ≤ count ≤ 12 (§11.1)
2. IDs are unique, non-empty, and use only ASCII letters, digits, `-`, or `_`
3. Every task has ≥1 acceptance criterion AND ≥1 gate
4. `source` starts with `issue:`, `pr:`, or `backlog:` (real-reference shape only)

> **Note (C4):** `validate` checks the reference SHAPE only. Corpus authenticity —
> confirming each committed task is genuine repo backlog, not a toy example — is
> verified manually at PR review.

**Content hash:** The frozen corpus hash is recorded in the PR body / merge tag.
It is NEVER embedded in the corpus file itself (avoids self-referential hash).

---

## Runner commands

```bash
# Build the crate first (from repo root or benchmarks/abeval/)
cargo build --manifest-path benchmarks/abeval/Cargo.toml

# Validate the corpus and print its content hash
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- validate

# Run one corpus task through both arms in dry-run (default, no network/model).
# --budget-usd is optional; it is recorded in run_meta.json but does NOT by
# itself enable live execution (see the cost-approval rule below).
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- run \
  --task abeval-01-issue-95 --arms both --dry-run --out /tmp/abeval-run \
  [--budget-usd 5.0]

# Summarize the run and enforce the §11.3 headline gate
cargo run --manifest-path benchmarks/abeval/Cargo.toml -- report --run /tmp/abeval-run
```

---

## Artifact layout

After `abeval run --task <id> --arms both --out <dir>`:

```
<dir>/
  <task_id>/
    run_meta.json        # task_id, arms, dry_run, approved_paid_run, evidence_class, budget_usd, per_arm usage
    ironmem/
      usage.json         # token counts for the ironmem arm
      transcript.txt     # execution transcript (synthesized in dry-run)
    superpowers/
      usage.json
      transcript.txt
```

Key fields in `run_meta.json`:
- `task_id: string` — the corpus task this run covers
- `arms: string[]` — arm labels executed (e.g. `["ironmem", "superpowers"]`)
- `dry_run: bool` — true for the default smoke path
- `approved_paid_run: bool` — false unless explicit approval was given
- `evidence_class: "smoke" | "live"` — dry-run artifacts are always `"smoke"`
- `budget_usd: number | null` — the optional `--budget-usd` ceiling, recorded for
  audit only (it does not by itself enable live execution)
- `per_arm: [{ arm, outcome, usage }]` — per-arm token usage and outcome

---

## Cost-approval rule and fail-closed live path

**Default mode is always dry-run** (no network, no model, no agent spawn).

Live execution requires BOTH:
1. The explicit `--execute-live` flag
2. A cost-approval opt-in, either:
   - env var `ABEVAL_PAID_RUN_APPROVED` set to any of (case-insensitive, trimmed):
     `1`, `true`, `yes`, `y`, `approve`, `approved`; or
   - `--approval-file <path>` whose **entire (trimmed) content** is exactly:
     `I approve paid A/B runs`

The optional `--budget-usd <amount>` flag is an audit-only ceiling recorded in
`run_meta.json`; it never enables live execution on its own.

If `--execute-live` is set without the approval opt-in, the runner returns a clear
error **before constructing or spawning any process**. The guard is checked before any
executor is built — no process is ever started without explicit approval.

This is a HARD COST RULE from issue #97: any actual A/B run is paid and needs explicit
user cost approval first.

---

## superpowers arm isolation (C1)

The `superpowers` arm working context must be isolated from ironmem server-side state:

- **NO** `/collab` command or planning session
- **NO** semantic search (ironmem `search` tool)
- **NO** KG reads/writes (`kg_add`, `kg_query`, etc.)
- **NO** drawer reads/writes
- **NO** other ironmem server-side memory state in the working context

The inert `LiveExecutor` command template includes these prohibitions in the task prompt.
**The prompt prefix is NOT the enforcement boundary** — a model can ignore an instruction.
Any future live runner MUST enforce C1 by *environment isolation*: launch the `superpowers`
arm in a harness configuration that physically omits the ironmem MCP server and starts from
clean state (no ironmem state dir), so the arm cannot reach ironmem even if it tried. The
prompt prefix is belt-and-suspenders only. Task_tag or reporting instrumentation (needed to
attribute token rows in the ironmem DB) is **measurement-only** and kept strictly separate
from the arm's working context.

This separation is required by METRICS_SPEC §11.2 and the C1 clarification from the
Codex review of the canonical plan.

---

## Interpretation limits

- Dry-run output is `evidence_class: "smoke"` and is explicitly **non-headline**. No
  cross-arm delta is claimed from smoke runs.
- Headline deltas require **≥8 merged+CI-green tasks per arm** from **live evidence**
  and are always reported confidence-qualified (n + spread at minimum — never a bare
  point estimate).
- A task counts toward the headline gate ONLY when `outcome == "merged"` AND CI-green
  (§2.2). Failed, abandoned, or smoke attempts are visible in the report (with their
  spend) but never count toward the gate.
- `abeval validate` enforces the real-reference SHAPE of `source` fields. Corpus
  authenticity is verified manually at PR review (C4).

---

## Cross-references

- `docs/METRICS_SPEC.md §11` — the frozen measurement contract this harness serves
- `docs/METRICS_SPEC.md §2` — token accounting definitions (tokens-to-done §2.1,
  done semantics §2.2, task_key §2.3)
- `benchmarks/provbench/baseline/` — reference crate this harness mirrors
