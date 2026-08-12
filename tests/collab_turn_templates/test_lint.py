import os, pathlib, re, shutil, subprocess, sys

import pytest

ROOT = pathlib.Path(__file__).resolve().parents[2]
LINT = ROOT / "scripts" / "check_collab_turn_templates.py"
MARK = "MUTATED-BY-TEST"


def run(extra_env=None):
    env = os.environ.copy()
    if extra_env:
        env.update(extra_env)
    return subprocess.run([sys.executable, str(LINT)], cwd=ROOT, env=env,
                          capture_output=True, text=True)


def copy_fixture(tmp_path):
    """A fixture tree the lint passes cleanly on.

    Every surface the lint cross-references is copied, so `test_fixture_is_green`
    below holds and every `assert r.returncode == 1` in this file means "the
    mutation broke it", not "the fixture was already incomplete". The fixture
    used to omit `scripts/install-ironmem.sh` and the Rust sources, so it exited
    1 untouched and those assertions proved nothing on their own.
    """
    fixture = tmp_path / "repo"
    for rel in (".claude-plugin/prompts", ".claude-plugin/commands",
                # Without the Codex commands dir the Codex-side command checks
                # never run in any fixture test, which is how ~22 per-prompt
                # contract pins sat unexercised behind
                # `if not CODEX_COMMAND.exists()`.
                ".codex-plugin/prompts", ".codex-plugin/commands"):
        shutil.copytree(ROOT / rel, fixture / rel)
    (fixture / "docs").mkdir()
    shutil.copy2(ROOT / "docs" / "COLLAB.md", fixture / "docs" / "COLLAB.md")
    shutil.copy2(ROOT / "docs" / "EVALUATE_ISSUE.md", fixture / "docs" / "EVALUATE_ISSUE.md")
    (fixture / "scripts").mkdir()
    shutil.copy2(ROOT / "scripts" / "install-ironmem.sh",
                 fixture / "scripts" / "install-ironmem.sh")
    # The review-lens mutation-classification check (task 11) reads the
    # ROSTER straight out of ultrareview.js — without this the check has no
    # source of truth to compare prompt/command prose against and every
    # fixture run fails on "missing" rather than exercising the check.
    (fixture / ".claude-plugin" / "workflows").mkdir(parents=True)
    shutil.copy2(ROOT / ".claude-plugin" / "workflows" / "ultrareview.js",
                 fixture / ".claude-plugin" / "workflows" / "ultrareview.js")
    # The failure-prefix and phase-name checks derive their valid sets from
    # the Rust sources rather than from hardcoded copies.
    rust = pathlib.Path("crates") / "ironmem" / "src" / "collab"
    (fixture / rust).mkdir(parents=True)
    for name in ("mod.rs", "phase.rs"):
        shutil.copy2(ROOT / rust / name, fixture / rust / name)
    return fixture


def mutate(path, snippet, replacement=MARK):
    """Replace EVERY occurrence of `snippet` in `path`.

    Every occurrence, not just the first: a pin whose text appears more than
    once in its file survives a first-occurrence-only mutation, and the test
    would then pass for the wrong reason — it would prove the lint stayed
    green, not that it fired.
    """
    text = path.read_text()
    assert snippet in text, f"target not found in {path.name}: {snippet!r}"
    path.write_text(text.replace(snippet, replacement))


def mutate_flex(path, snippet, replacement=MARK):
    """Replace a lint `flex()` phrase despite prose line wrapping."""
    text = path.read_text()
    pattern = r"\s+".join(re.escape(part) for part in snippet.split())
    mutated, count = re.subn(pattern, replacement, text)
    assert count, f"flex target not found in {path.name}: {snippet!r}"
    path.write_text(mutated)


def mutate_once(path, snippet, replacement):
    """Replace one occurrence so repeated ceiling text cannot hide drift."""
    text = path.read_text()
    assert snippet in text, f"target not found in {path.name}: {snippet!r}"
    path.write_text(text.replace(snippet, replacement, 1))


# The lint's contract lists, duplicated here on purpose. Importing them from
# the lint would make these tests parametrize over "whatever the lint currently
# pins", so deleting an entry would silently shrink the sweep instead of
# failing it. Every entry below must independently red the gate.
CODEX_PILOT_ROUTING_SNIPPETS = [
    "`PlanSynthesisPending` with normal Codex-pilot ownership",
    "`PlanClaudeFinalizePending` with normal Codex-pilot ownership",
    "`CodeReviewLocalPending` with normal Codex-pilot ownership",
    "`CodeReviewFinalPending` with normal Codex-pilot ownership",
]
COMPOSE_HANDOFF_SNIPPETS = [
    "this is a **normal compose\n      handoff**, not a dispatch failure",
    "`$TOPIC=final`,\n        `$ARTIFACT_REF=<drawer_id>`, and\n"
    "        `$SENDER=<collab_status.current_owner>`.",
    "`$TOPIC=final_review`, `$ARTIFACT_REF=<drawer_id>`, and\n"
    "        `$SENDER=<collab_status.current_owner>`.",
    "Do\n      not re-dispatch the compose prompt, and do not emit\n      `codex_dispatch_failed:`",
]
TASK_LIST_BRIDGE_SNIPPETS = [
    "`collab-turn-task-list.md` once (mechanical/sonnet) with\n"
    "`$SENDER=<collab_status.current_owner>`.",
    "dispatch `collab-turn-task-list.md`\n   (mechanical/sonnet) with "
    "`$SENDER=<collab_status.current_owner>`.",
    "it\n   must never be hardcoded to `\"claude\"`",
]
DISPATCH_FAILURE_ADMISSIBILITY_SNIPPETS = [
    "**Dispatch-failure-admitting phases only** — `CodeImplementPending`\n"
    "        (when `implementer == \"codex\"`) and "
    "`CodeReviewFixGlobalPending`:",
    "**Every other phase** — the planning phases (`PlanParallelDrafts`,",
    "additionally requires `dispatch_failure_phase_admits`\n"
    "        (`crates/ironmem/src/collab/mod.rs`), which returns `false` "
    "for both —",
    "**Every other phase:** as in condition 6 — the planning phases are",
]
DOC_PR_BASE_SNIPPETS = [
    "does **not** require that branch to contain `base_sha`",
    "pre-range commits in the PR body",
]
DOC_PILOT_SUBMIT_SNIPPETS = [
    "$SENDER` where that template uses",
]
SUBMIT_TEMPLATE_SNIPPETS = [
    "parse the artifact JSON as",
    "gh pr create --base <base_branch>",
    'collab_send(sender="$SENDER", topic="final_review",',
    'collab_send(sender="$SENDER", topic="final",',
    'collab_send(sender="$SENDER",\n  topic="failure_report",\n'
    '  content=<JSON {"coding_failure":"pr_create_failed:',
    'collab_send(sender="$SENDER",\n  topic="failure_report", content=<JSON'
    ' {"coding_failure":\n  "approved_artifact_unfetchable:',
    'Verify `$SENDER` against `collab_status.current_owner`',
    'MUST NOT be\n   substituted with your own identity',
    'may\n     legitimately be the recovery owner rather than the pilot',
    'equal `current_owner`, ABORT — do not send anything — and report the',
]
TASK_LIST_TEMPLATE_SNIPPETS = [
    "Timebox: <=20 minutes",
    "more than 15 tasks",
    "PlanLocked is pre-coding",
    "plan_file_path",
    'collab_send(sender="$SENDER", topic="task_list",',
    'Verify `$SENDER` against `collab_status.current_owner`',
    'MUST NOT be\n   substituted with your own identity',
    'equal `current_owner`, ABORT — do not send anything — and report the',
    'always the pilot, which under `pilot == "codex"` is `codex`',
]
# Deliberately independent from the linter's map: importing production pins
# here would let deleting a checker entry silently shrink the mutation sweep.
TASK_BUDGET_SURFACE_SNIPPETS = {
    "docs/COLLAB.md": [
        "**1–15 execution tasks**",
        "1–15-task collab session",
        "A plan projected to require 16 or more tasks",
        "more than 15 tasks",
        "`> 15` task-count check",
        "A 16+ task issue",
        "**1–15** strictly ordered entries",
    ],
    "docs/EVALUATE_ISSUE.md": [
        "An estimate above 15 requires `SPLIT`.",
        "more than 15 independent execution tasks",
        "**1–15** execution tasks",
        "1–15 task estimate",
        "An estimate above 15 tasks always yields `SPLIT`",
    ],
    ".claude-plugin/commands/collab.md": [
        "at most 15 tasks",
        "If it would need 16 or more",
        "`> 15` task-count check",
        "a 16-task plan",
        "more than 15 `### Task ` headings",
    ],
    ".claude-plugin/commands/evaluate-issue.md": [
        "mandatory SPLIT above 15 tasks",
        "above 15 requires `SPLIT`",
        "more than 15 independent execution tasks",
        "1–15-task estimate",
        "1–15 task estimate",
        "estimate above 15 tasks always yields `SPLIT`",
    ],
    ".claude-plugin/prompts/collab-turn-plan-review.md": [
        "capped at 15 execution tasks",
        "credibly needs 16 or more",
    ],
    ".claude-plugin/prompts/collab-turn-plan-draft.md": [
        "at most 15 execution tasks",
    ],
    ".claude-plugin/prompts/collab-turn-plan-synthesis.md": [
        "at most 15 execution tasks",
    ],
    ".claude-plugin/prompts/collab-turn-plan-finalize.md": [
        "at most 15 tasks",
        "needs 16 or more",
        "heading count is at most 15",
    ],
    ".claude-plugin/prompts/collab-turn-task-list.md": [
        "heading count is at most 15",
        "more than 15 tasks",
    ],
    ".codex-plugin/commands/collab.md": [
        "1–15 execution tasks",
        "work needs 16 or more",
    ],
    ".codex-plugin/prompts/evaluate-issue.md": [
        "mandatory SPLIT above 15 tasks",
        "above 15 requires `SPLIT`",
        "more than 15 independent execution tasks",
        "1–15-task estimate",
        "1–15 task estimate",
        "estimate above 15 tasks always yields `SPLIT`",
    ],
    ".codex-plugin/prompts/collab-plan-draft.md": [
        "at most 15 execution tasks",
    ],
    ".codex-plugin/prompts/collab-plan-synthesis.md": [
        "at most 15 execution tasks",
    ],
    ".codex-plugin/prompts/collab-plan-review.md": [
        "capped at 15 execution tasks",
        "credibly needs 16 or more",
    ],
    ".codex-plugin/prompts/collab-plan-finalize.md": [
        "at most 15 tasks",
        "needs 16 or more",
        "at least 1 and at most 15",
    ],
    ".codex-plugin/prompts/collab-task-list.md": [
        "at most 15",
        "more than 15 tasks",
    ],
}
TASK_BUDGET_SURFACE_CASES = [
    (path, snippet)
    for path, snippets in TASK_BUDGET_SURFACE_SNIPPETS.items()
    for snippet in snippets
]
TASK_BUDGET_STALE_DRIFT_CASES = [
    ("docs/COLLAB.md",
     "minutes or any scope that credibly needs more than 15 tasks must be called out",
     "more than 15 tasks"),
    ("docs/COLLAB.md",
     "contain 1–15 tasks. If it would need 16 or more, stop before sending `final`",
     "1–15 tasks"),
    ("docs/COLLAB.md",
     "that the task list contains 1–15 tasks",
     "1–15 tasks"),
    ("docs/COLLAB.md", "its own 1–15-task collab session", "1–15-task collab session"),
    ("docs/COLLAB.md", "**1–15** strictly ordered entries",
     "**1–15** strictly ordered entries"),
    ("docs/COLLAB.md", "A 16+ task issue", "16+ task"),
    ("docs/EVALUATE_ISSUE.md", "collab's 15-task issue budget", "15-task issue budget"),
    ("docs/EVALUATE_ISSUE.md", "An estimate above 15 requires `SPLIT`.",
     "above 15 requires"),
    ("docs/EVALUATE_ISSUE.md", "An estimate above 15 tasks always yields `SPLIT`",
     "estimate above 15 tasks"),
    ("docs/EVALUATE_ISSUE.md",
     "1. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>",
     "1–15 task estimate"),
    (".claude-plugin/commands/evaluate-issue.md",
     "1. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>",
     "1–15 task estimate"),
    (".codex-plugin/prompts/evaluate-issue.md",
     "1. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>",
     "1–15 task estimate"),
    (".codex-plugin/commands/collab.md", "1–15 execution tasks", "1–15 execution tasks"),
    (".claude-plugin/commands/collab.md",
     "there are more than 15 `### Task ` headings",
     "more than 15 `### Task ` headings"),
    (".claude-plugin/commands/collab.md", "`> 15` task-count check",
     "> 15` task-count"),
    (".claude-plugin/commands/collab.md", "a 16-task\n   plan", "16-task plan"),
    (".claude-plugin/prompts/collab-turn-plan-review.md",
     "credibly needs 16 or more", "needs 16 or more"),
    (".claude-plugin/prompts/collab-turn-plan-finalize.md", "at most 15 tasks",
     "at most 15 tasks"),
    (".claude-plugin/prompts/collab-turn-plan-finalize.md",
     "heading count is at most 15", "heading count is at most 15"),
    (".claude-plugin/prompts/collab-turn-task-list.md",
     "more than 15 tasks", "more than 15 tasks"),
    (".codex-plugin/prompts/collab-plan-review.md",
     "credibly needs 16 or more", "needs 16 or more"),
    (".codex-plugin/prompts/collab-plan-finalize.md",
     "If it needs 16 or more", "needs 16 or more"),
    (".codex-plugin/prompts/collab-plan-finalize.md",
     "at least 1 and at most 15", "at least 1 and at most 15"),
    (".codex-plugin/prompts/collab-task-list.md",
     "more than 15 tasks", "more than 15 tasks"),
]

# The three-role checks added for pilot configurability are intentionally
# duplicated here instead of imported from the lint.  These tests are the
# proof that each checker is live: deleting a phrase from the checker itself
# must not silently shrink the fixture-mutation sweep along with it.
PILOT_FLAG_SECTION_SNIPPETS = {
    "start": "Strip both flag tokens out of the stream before capturing the positional `<task>`",
    "review": "Never set `initiator` from the `--pilot` value",
    "join": "Strip both flag tokens out of the stream before capturing the positional `<session_id>`",
}
JOIN_PILOT_SURFACES = [
    (".claude-plugin/commands/collab.md",
     "**Requested pilot matches `status.pilot`** → no-op",
     ".claude-plugin/commands/collab.md: missing join pilot-authorization contract"),
    (".codex-plugin/commands/collab.md",
     "**Requested pilot matches `status.pilot`** → no-op",
     ".codex-plugin/commands/collab.md: missing join pilot-authorization contract"),
]
PLAN_LOCKED_GATE_SNIPPET = "**Dispatcher-owned planning approval gate.**"
PLAN_FINALIZE_ROW_SNIPPET = "**No human gate here — this turn is autonomous.**"
SINGLE_PILOT_RESOLUTION_SNIPPET = "No call site may re-derive role identity from a phase name, a prompt filename, or a value remembered from a prior iteration"
DOC_PILOT_CONTRACT_SNIPPETS = [
    "- **dispatcher** — runs the control loop shown above and is the only role that talks to the human",
    "| `pilot` | Which agent leads v1 planning and the v3 review-audit turns",
    "### `collab_set_pilot`",
    "**Wire-compat note:**",
    "### Codex-terminal-led sessions are a non-goal",
    "**Pilot-configurability is not a step toward an N-party protocol.**",
]


def test_fixture_is_green(tmp_path):
    # The premise every `assert r.returncode == 1` below depends on.
    fixture = copy_fixture(tmp_path)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 0, f"unmutated fixture must lint clean:\n{r.stdout}\n{r.stderr}"


def test_lint_passes_on_repo():
    r = run()
    assert r.returncode == 0, f"lint failed:\n{r.stdout}\n{r.stderr}"


@pytest.mark.parametrize("section,snippet", PILOT_FLAG_SECTION_SNIPPETS.items())
def test_lint_requires_pilot_flag_contract_in_each_entry_section(tmp_path, section, snippet):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    mutate_flex(command, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f".claude-plugin/commands/collab.md: `{section}` section is "
            f"missing pilot-flag parsing contract {snippet!r}") in r.stdout


@pytest.mark.parametrize("path,snippet,error", JOIN_PILOT_SURFACES)
def test_lint_requires_pilot_join_authorization_on_both_harnesses(
        tmp_path, path, snippet, error):
    fixture = copy_fixture(tmp_path)
    mutate(fixture / path, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{error} {snippet!r}" in r.stdout


def test_lint_requires_dispatcher_planlocked_gate_and_autonomous_finalize(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    mutate(command, PLAN_LOCKED_GATE_SNIPPET)
    mutate(command, PLAN_FINALIZE_ROW_SNIPPET)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "v3 bridge is missing dispatcher approval-gate contract" in r.stdout
    assert "PlanClaudeFinalizePending` row must state that the turn is autonomous" in r.stdout


def test_lint_rejects_a_planlocked_row_in_the_codex_shim(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".codex-plugin" / "commands" / "collab.md"
    table_header = "| Phase | Prompt |\n|---|---|"
    assert table_header in command.read_text()
    command.write_text(command.read_text().replace(
        table_header,
        table_header + "\n| `PlanLocked` | `collab-task-list.md` |",
        1,
    ))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "phase→prompt table must never carry a `PlanLocked` row" in r.stdout


def test_lint_requires_single_pilot_resolution_contract(tmp_path):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / ".claude-plugin" / "commands" / "collab.md",
                SINGLE_PILOT_RESOLUTION_SNIPPET)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("dispatch loop is missing single-pilot-resolution contract "
            f"{SINGLE_PILOT_RESOLUTION_SNIPPET!r}") in r.stdout


@pytest.mark.parametrize("snippet", DOC_PILOT_CONTRACT_SNIPPETS)
def test_lint_requires_each_pilot_documentation_contract(tmp_path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / "docs" / "COLLAB.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "docs/COLLAB.md: missing" in r.stdout
    assert snippet in r.stdout


def test_review_paths_prefer_artifact_and_preserve_raw_diff_fallback():
    global_review = (ROOT / ".codex-plugin" / "prompts" /
                     "collab-global-review.md").read_text()
    local_review = (ROOT / ".claude-plugin" / "prompts" /
                    "collab-turn-review-local.md").read_text()
    ultra_review = (ROOT / ".claude-plugin" / "commands" /
                    "ultrareview-local.md").read_text()

    for surface in (global_review, local_review, ultra_review):
        assert "ironmem review-diff" in surface
        assert "--expand-file <path> --hunk <ordinal>" in surface
        assert "git diff" in surface
        assert "only on success" in surface

    assert "--repo <repo_path> --base <base_sha> --head <last_head_sha>" in global_review
    assert "--repo <repo_path> --base <base_sha> --head <last_head_sha>" in local_review
    assert "gh pr diff <N>" in ultra_review
    assert "--worktree" in ultra_review


def test_ultrareview_uses_a_transient_raw_diff_for_complete_trigger_detection():
    ultra_review = (ROOT / ".claude-plugin" / "commands" /
                    "ultrareview-local.md").read_text()

    assert "Preserve the full raw diff transiently for deterministic trigger detection" in ultra_review
    assert "Do not inject or repeat that raw diff in reviewer prompts" in ultra_review


def test_lint_requires_review_diff_fallback_contract(tmp_path):
    fixture = copy_fixture(tmp_path)
    prompt = fixture / ".codex-plugin" / "prompts" / "collab-global-review.md"
    prompt.write_text(prompt.read_text().replace("only on success", "on success", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "missing review-diff fallback contract" in r.stdout


def test_lint_requires_raw_trigger_detection_contract(tmp_path):
    fixture = copy_fixture(tmp_path)
    prompt = fixture / ".claude-plugin" / "commands" / "ultrareview-local.md"
    prompt.write_text(prompt.read_text().replace(
        "Preserve the full raw diff transiently for deterministic trigger detection",
        "Use the diff for trigger detection",
        1,
    ))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "missing review-diff trigger-detection contract" in r.stdout


def test_plan_and_manifest_workers_use_verified_references():
    task_list = (ROOT / ".claude-plugin" / "prompts" / "collab-turn-task-list.md").read_text()
    batch = (ROOT / ".codex-plugin" / "prompts" / "collab-batch-impl.md").read_text()

    assert "final_plan_ref.plan_file_path" in task_list
    assert "SHA-256 equals both `final_plan_ref.hash` and `final_plan_hash`" in task_list
    assert "recreate it from the exact `final_plan` body" not in task_list
    assert "get_drawer(id=<task_list_ref.drawer_id>)" in batch
    assert "verify its SHA-256 against\n`task_list_ref.hash`" in batch
    assert "do not request `include_task_list`" in batch


def test_codex_dispatch_uses_explicit_repository_model_defaults():
    docs = (ROOT / "docs" / "COLLAB.md").read_text()
    dispatcher = (ROOT / ".claude-plugin" / "commands" / "collab.md").read_text()
    # Two surfaces this assertion used to cover are gone, and neither is
    # replaced. The bundled skills no longer carry a codex-tools.md reference
    # sheet -- the iron-* skills resolve harness tool names from
    # skills/vocab.toml at generation time. AGENTS.md was deleted outright by
    # PR #240 (adb5c80); the read of it left this suite red from that merge
    # until now, unnoticed because nothing ran the suite.

    codex_prompts = {p.name: p.read_text()
                     for p in sorted((ROOT / ".codex-plugin" / "prompts").glob("collab-*.md"))}
    assert len(codex_prompts) >= 7, f"expected every collab-*.md prompt, got {sorted(codex_prompts)}"
    plan_draft_prompt = codex_prompts["collab-plan-draft.md"]

    for name, surface in [("docs/COLLAB.md", docs), (".claude-plugin/commands/collab.md", dispatcher),
                          *codex_prompts.items()]:
        assert "gpt-5.6-luna" in surface, name
        assert "gpt-5.6-terra" in surface, name
        assert "gpt-5.6-sol" in surface, name

    assert "CodeImplementPending" in dispatcher
    assert "-m gpt-5.6-luna -c model_reasoning_effort=max" in dispatcher
    assert "-m gpt-5.6-terra -c model_reasoning_effort=high" in dispatcher
    assert "codex exec --prompt-file" not in dispatcher
    assert "model_reasoning_effort=xhigh" not in dispatcher
    assert "personal Codex default" in docs
    assert "escalation tier, not the default" in plan_draft_prompt


def test_lint_requires_logical_keyed_checkpoint_contract(tmp_path):
    fixture = copy_fixture(tmp_path)
    bad = fixture / ".codex-plugin" / "prompts" / "collab-batch-impl.md"
    bad.write_text(bad.read_text().replace(
        "collab-checkpoint:<session_id>", "missing-checkpoint-key"
    ))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "missing checkpoint contract 'collab-checkpoint:<session_id>'" in r.stdout


def test_codex_background_dispatcher_uses_quiet_settled_waits():
    docs = (ROOT / "docs" / "COLLAB.md").read_text()
    dispatcher = (ROOT / ".claude-plugin" / "commands" / "collab.md").read_text()

    for surface in (docs, dispatcher):
        assert 'collab_wait_my_turn(session_id, "claude", 60)' in surface
        assert '{"unchanged": true}' in surface
        assert "consecutive-duplicate collapsing" in surface
        assert "settled full frame" in surface
        assert "actionable post-claim session-state change" in surface
        assert "recovery-state changes" in surface

    assert "On each iteration:" not in dispatcher
    assert "Call `mcp__ironmem__collab_status(session_id)` to detect phase advance." not in dispatcher
    assert "[codex bg] <last stdout line>" not in dispatcher
    assert "no state transition" not in docs
    assert "no state transition" not in dispatcher
    assert "Every time a poll observes a new phase" not in docs
    assert "polling loop exits" not in dispatcher


def test_lint_catches_unknown_placeholder(tmp_path):
    fixture = copy_fixture(tmp_path)
    bad = fixture / ".claude-plugin" / "prompts" / "collab-turn-plan-draft.md"
    bad.write_text(bad.read_text() + "\nUse $NOT_ALLOWED here.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "unknown placeholder $NOT_ALLOWED" in r.stdout


def test_lint_catches_bad_verdict_schema(tmp_path):
    fixture = copy_fixture(tmp_path)
    bad = fixture / ".claude-plugin" / "prompts" / "collab-turn-submit.md"
    bad.write_text(bad.read_text().replace("blocker:", "blocked:", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "verdict block must be result/ref/blocker lines" in r.stdout


def test_lint_catches_stale_planning_direct_body_claim(tmp_path):
    fixture = copy_fixture(tmp_path)
    bad = fixture / ".claude-plugin" / "prompts" / "collab-turn-plan-synthesis.md"
    bad.write_text(bad.read_text() + "\nread Codex's draft\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "forbidden stale direct-body claim" in r.stdout


def test_lint_requires_planning_body_dereferences(tmp_path):
    fixture = copy_fixture(tmp_path)
    required_refs = [
        ("collab-turn-plan-synthesis.md", "get_drawer(id=<message.drawer_id>)"),
        ("collab-turn-plan-finalize.md", "get_drawer(id=<canonical_plan_ref.drawer_id>)"),
    ]
    for name, ref in required_refs:
        path = fixture / ".claude-plugin" / "prompts" / name
        text = path.read_text()
        assert ref in text, f"expected dereference contract missing from fixture: {name}"
        path.write_text(text.replace(ref, "get_drawer(id=<removed>)", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    for _, ref in required_refs:
        assert f"missing required contract snippet {ref!r}" in r.stdout


def test_lint_catches_typoed_matrix_tier(tmp_path):
    # A typo in a matrix tier token parses to None. The cross-check must NOT
    # silently skip it; it must record a specific "unrecognized tier/model"
    # error against the named template.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = cmd.read_text()
    # The submit row: `| post-gate send | claude | `collab-turn-submit.md` |
    # mechanical | sonnet |` — corrupt the tier token only.
    mutated = text.replace(
        "| `collab-turn-submit.md` | mechanical | sonnet |",
        "| `collab-turn-submit.md` | mechnical | sonnet |",
        1,
    )
    assert mutated != text, "matrix submit row not found to mutate"
    cmd.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "matrix row for collab-turn-submit.md: unrecognized tier/model" \
        in r.stdout


def test_lint_requires_evaluate_issue_split_contract(tmp_path):
    fixture = copy_fixture(tmp_path)
    prompt = fixture / ".codex-plugin" / "prompts" / "evaluate-issue.md"
    prompt.write_text(prompt.read_text().replace("Child issues:", "Child work:", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ".codex-plugin/prompts/evaluate-issue.md: missing evaluate-issue SPLIT contract" \
        in r.stdout


def test_lint_requires_fifteen_task_evaluate_issue_ceiling(tmp_path):
    fixture = copy_fixture(tmp_path)
    prompt = fixture / ".codex-plugin" / "prompts" / "evaluate-issue.md"
    text = prompt.read_text()
    stale_ceiling = "more than " + "10 independent execution tasks"
    mutated = text.replace(
        "more than 15 independent execution tasks",
        stale_ceiling,
        1,
    )
    assert mutated != text, "15-task evaluate-issue ceiling not found"
    prompt.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ".codex-plugin/prompts/evaluate-issue.md: missing evaluate-issue SPLIT contract" \
        in r.stdout


def test_lint_requires_sixteen_task_bridge_ceiling(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = command.read_text()
    stale_ceiling = "an " + "11" + "-task plan"
    mutated = text.replace("a 16-task\n   plan", stale_ceiling, 1)
    assert mutated != text, "16-task bridge ceiling not found"
    command.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".claude-plugin/commands/collab.md: v3 bridge is missing "
            "blocker-terminates-the-bridge contract 'a 16-task plan'") in r.stdout


@pytest.mark.parametrize("path,snippet", TASK_BUDGET_SURFACE_CASES)
def test_lint_requires_every_task_budget_surface_contract(tmp_path, path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / path, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{path}: missing task-budget contract {snippet!r}" in r.stdout


@pytest.mark.parametrize("path,context,canonical", TASK_BUDGET_STALE_DRIFT_CASES)
def test_lint_rejects_each_single_occurrence_stale_task_budget_drift(
        tmp_path, path, context, canonical):
    fixture = copy_fixture(tmp_path)
    stale_context = context.replace("15", "10").replace("16", "11")
    stale = canonical.replace("15", "10").replace("16", "11")
    mutate_once(fixture / path, context, stale_context)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{path}: stale task-budget ceiling {stale!r}; "
            "required 15/16 contract") in r.stdout


@pytest.mark.parametrize(
    "path,context,replacement,canonical,expected_count",
    [
        (
            ".codex-plugin/prompts/evaluate-issue.md",
            "1. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>",
            "1. <title> — <scope, acceptance summary, 1–14 task estimate, dependencies>",
            "1–15 task estimate",
            2,
        ),
        (
            "docs/COLLAB.md",
            "minutes or any scope that credibly needs more than 15 tasks must be called out",
            "minutes or any scope that credibly needs more than 20 tasks must be called out",
            "more than 15 tasks",
            3,
        ),
    ],
)
def test_lint_rejects_nonlegacy_single_occurrence_task_budget_drift(
        tmp_path, path, context, replacement, canonical, expected_count):
    fixture = copy_fixture(tmp_path)
    mutate_once(fixture / path, context, replacement)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{path}: task-budget contract {canonical!r} expected "
            f"{expected_count} occurrences, found {expected_count - 1}") in r.stdout


def test_lint_scopes_task_budget_enforcement_to_planlocked_bridge(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = command.read_text()
    heading = "## v3 Bridge: PlanLocked → CodeImplementPending"
    prefix, bridge = text.split(heading, 1)
    enforcement = "more than 15 `### Task ` headings"
    pattern = r"\s+".join(re.escape(part) for part in enforcement.split())
    mutated_bridge, count = re.subn(pattern, MARK, bridge, count=1)
    assert count == 1, "PlanLocked bridge task-budget enforcement not found"
    # Preserve the whole-file positive pin outside the bridge. Only the
    # section-specific checker can reject this mutation.
    command.write_text(prefix + enforcement + "\n\n" + heading + mutated_bridge)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".claude-plugin/commands/collab.md: PlanLocked bridge is missing "
            "task-budget enforcement 'more than 15 `### Task ` headings'") in r.stdout


FINALIZE_ABORT_SURFACES = [
    (
        ".claude-plugin/prompts/collab-turn-plan-finalize.md",
        "any blocker that prevents staging a valid final plan must call "
        "`collab_end(session_id=$SESSION_ID, agent=\"claude\")` exactly once",
    ),
    (
        ".codex-plugin/prompts/collab-plan-finalize.md",
        "any blocker that prevents staging a valid final plan must call "
        "`collab_end(session_id, agent=\"codex\")` exactly once",
    ),
    (
        ".claude-plugin/prompts/collab-turn-submit.md",
        "call `collab_end(session_id=$SESSION_ID, agent=\"$SENDER\")` "
        "before returning the blocker",
    ),
    (
        ".claude-plugin/commands/collab.md",
        "A finalization `blocker:` is terminal: the worker must have ended "
        "the session",
    ),
    (
        "docs/COLLAB.md",
        "A finalization `blocker:` is terminal: the worker must have ended "
        "the session",
    ),
]


@pytest.mark.parametrize("path,snippet", FINALIZE_ABORT_SURFACES)
def test_lint_requires_finalize_blocker_to_end_session(tmp_path, path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / path, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{path}: missing finalize-abort contract {snippet!r}" in r.stdout


def test_lint_requires_both_harness_templates_per_topic(tmp_path):
    fixture = copy_fixture(tmp_path)
    (fixture / ".codex-plugin" / "prompts" / "collab-review-local.md").unlink()

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "missing Codex prompt collab-review-local.md" in r.stdout


def test_lint_requires_claude_copilot_templates(tmp_path):
    fixture = copy_fixture(tmp_path)
    (fixture / ".claude-plugin" / "prompts" / "collab-turn-plan-review.md").unlink()

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "missing Claude template collab-turn-plan-review.md" in r.stdout


def test_lint_requires_installer_to_cover_every_template(tmp_path):
    fixture = copy_fixture(tmp_path)
    installer = fixture / "scripts" / "install-ironmem.sh"
    installer.write_text(
        installer.read_text().replace("  collab-turn-plan-review\n", "", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "REQUIRED_CLAUDE_PROMPTS is missing 'collab-turn-plan-review'" in r.stdout


def test_lint_rejects_a_precondition_naming_a_rust_variant(tmp_path):
    # `collab_status` emits `wire_name`, so a `preconditions:` line naming the
    # Rust variant of a genericized phase fails closed on every dispatch. The
    # valid set is parsed out of phase.rs, so the fixture needs that file.
    fixture = copy_fixture(tmp_path)
    template = fixture / ".claude-plugin" / "prompts" / "collab-turn-plan-review.md"
    template.write_text(template.read_text().replace(
        "preconditions: phase == PlanCodexReviewPending",
        "preconditions: phase == PlanCopilotReviewPending",
        1,
    ))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-plan-review.md: preconditions names phase "
            "'PlanCopilotReviewPending', which phase.rs never emits — use the "
            "wire name 'PlanCodexReviewPending'") in r.stdout


def test_lint_rejects_a_rust_variant_in_the_template_body(tmp_path):
    # The comparison a worker executes is the prose State-discovery step, not
    # the `preconditions:` metadata. Mutating the body alone leaves the
    # frontmatter clean AND still satisfies the "PlanCodexReviewPending"
    # snippet pin, so only a whole-file scan catches it.
    fixture = copy_fixture(tmp_path)
    template = fixture / ".claude-plugin" / "prompts" / "collab-turn-plan-review.md"
    text = template.read_text()
    body_ref = "verify\n   `phase == PlanCodexReviewPending` (the wire name for this turn"
    assert body_ref in text, "body State-discovery comparison not found to mutate"
    template.write_text(text.replace(
        body_ref, body_ref.replace("PlanCodexReviewPending",
                                   "PlanCopilotReviewPending"), 1))
    assert "preconditions: phase == PlanCodexReviewPending" in template.read_text(), \
        "frontmatter must stay clean — that is the whole point of this test"

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-plan-review.md: preconditions names phase "
            "'PlanCopilotReviewPending', which phase.rs never emits") in r.stdout
    # The snippet pin must NOT be what caught it: the surviving frontmatter
    # occurrence satisfies that substring test.
    assert "missing required contract snippet 'PlanCodexReviewPending'" not in r.stdout


def test_lint_rejects_a_prose_precondition_naming_no_phase(tmp_path):
    fixture = copy_fixture(tmp_path)
    template = fixture / ".claude-plugin" / "prompts" / "collab-turn-task-list.md"
    template.write_text(template.read_text().replace(
        "preconditions: phase == PlanLocked,", "preconditions: phase is PlanLocked,", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-task-list.md: preconditions mentions a phase but "
            "names none in the `phase == <WireName>` form") in r.stdout


def test_lint_requires_codex_prompt_contracts_without_the_codex_command(tmp_path):
    # These pins used to live inside the `else:` branch of
    # `if not CODEX_COMMAND.exists()`, so with the command file absent two
    # inverted protocol contracts linted green. They must fire on prompt
    # content alone.
    fixture = copy_fixture(tmp_path)
    shutil.rmtree(fixture / ".codex-plugin" / "commands")
    final_review = fixture / ".codex-plugin" / "prompts" / "collab-final-review.md"
    final_review.write_text(final_review.read_text().replace(
        "**This turn sends nothing and opens no PR.**",
        "This turn may open a PR.",
        1,
    ))
    task_list = fixture / ".codex-plugin" / "prompts" / "collab-task-list.md"
    task_list.write_text(task_list.read_text().replace(
        "SHA-256 equals both `final_plan_ref.hash` and",
        "SHA-256 is close enough to",
        1,
    ))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".codex-plugin/prompts/collab-final-review.md: missing required "
            "recovery/dispatch contract '**This turn sends nothing and opens "
            "no PR.**'") in r.stdout
    assert (".codex-plugin/prompts/collab-task-list.md: missing required "
            "recovery/dispatch contract 'SHA-256 equals both "
            "`final_plan_ref.hash` and'") in r.stdout


def test_lint_requires_codex_prompt_contracts_with_the_codex_command(tmp_path):
    # Same pins, command file present: the fixture must exercise the full path.
    fixture = copy_fixture(tmp_path)
    synthesis = fixture / ".codex-plugin" / "prompts" / "collab-plan-synthesis.md"
    synthesis.write_text(synthesis.read_text().replace(
        "get_drawer(id=<message.drawer_id>)", "get_drawer(id=<removed>)", 1))
    review_local = fixture / ".codex-plugin" / "prompts" / "collab-review-local.md"
    review_local.write_text(review_local.read_text().replace(
        "Send `collab_send` with sender `codex`, topic `review_local`,",
        "Send `collab_send` with sender `claude`, topic `review_local`,",
        1,
    ))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".codex-plugin/prompts/collab-plan-synthesis.md: missing required "
            "recovery/dispatch contract 'get_drawer(id=<message.drawer_id>)'") \
        in r.stdout
    assert (".codex-plugin/prompts/collab-review-local.md: missing required "
            "recovery/dispatch contract 'Send `collab_send` with sender "
            "`codex`, topic `review_local`,'") in r.stdout


def test_lint_rejects_a_stale_phase_name_in_a_codex_prompt(tmp_path):
    # The Codex prompts hardcode their phase in prose. Renaming a wire string
    # in phase.rs used to flag only the Claude template, because the Codex
    # phase names were compared against hardcoded literals in the lint that
    # rotted in lockstep with the prompts.
    fixture = copy_fixture(tmp_path)
    prompt = fixture / ".codex-plugin" / "prompts" / "collab-review-local.md"
    text = prompt.read_text()
    guard = "if phase is not `CodeReviewLocalPending`"
    assert guard in text, "Codex phase guard not found to mutate"
    prompt.write_text(text.replace(
        guard, "if phase is not `CodeReviewCopilotLocalPending`", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".codex-plugin/prompts/collab-review-local.md: phase guard names "
            "phase 'CodeReviewCopilotLocalPending', which phase.rs never "
            "emits") in r.stdout


def test_lint_rejects_a_codex_prompt_whose_phase_guard_vanished(tmp_path):
    # A prompt that drops or reformats its guard must fail loudly rather than
    # become silently unchecked.
    fixture = copy_fixture(tmp_path)
    prompt = fixture / ".codex-plugin" / "prompts" / "collab-plan-finalize.md"
    text = prompt.read_text()
    mutated = (text.replace("prompt is only for `PlanClaudeFinalizePending`",
                            "prompt is only for the finalize turn", 1)
                   .replace("if phase is not `PlanClaudeFinalizePending`",
                            "if the phase is wrong", 1))
    assert mutated != text, "Codex phase guards not found to mutate"
    prompt.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ".codex-plugin/prompts/collab-plan-finalize.md: no phase guard found" \
        in r.stdout


# Every prompt that orders a `git reset --hard`. A turn that died hard never
# sends `failure_report`, so `pending_failure` stays null and the next dispatch
# is a normal turn — the cleanliness precondition must bind whatever
# `pending_failure` says, on every one of these files.
RESET_GUARD_PROMPTS = [
    (".claude-plugin/prompts", "collab-turn-review-local.md"),
    (".claude-plugin/prompts", "collab-turn-review-fix-global.md"),
    (".codex-plugin/prompts", "collab-review-local.md"),
    (".codex-plugin/prompts", "collab-global-review.md"),
    (".codex-plugin/prompts", "collab-batch-impl.md"),
]


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_requires_a_cleanliness_precondition_on_every_reset(
        tmp_path, subdir, name):
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    text = template.read_text()
    assert "--porcelain" in text, f"{name}: no precondition to mutate"
    # replace-all: a pin surviving because only its first occurrence moved is
    # the failure mode `mutate()` above exists to prevent.
    template.write_text(text.replace("--porcelain", "--short"))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: no `git status --porcelain` cleanliness precondition "
            "guarding `git reset --hard`") in r.stdout


def phrase_re(phrase):
    """A fixed phrase, matched across any run of whitespace.

    These mutations target prose inside wrapped paragraphs, so the line breaks
    inside a clause move whenever a word is added earlier in its sentence. A
    literal `assert snippet in text` then fails as "target not found" — the
    test breaking on a reflow rather than on a behaviour change. Duplicated
    from the lint's own helper on purpose, for the same reason the snippet
    lists above are duplicated.
    """
    return re.compile(r"\s+".join(re.escape(w) for w in phrase.split()))


# The guard's clauses as the shipped prompts wrap them today — matched by
# phrase, not by literal, for the reason above.
RESET_QUALIFIER_RE = phrase_re("regardless of `pending_failure`")
RESET_ENFORCEMENT_RE = phrase_re("do not run `git reset --hard`")
RESET_CONDITIONAL_RE = phrase_re("Only when the worktree is clean, `git reset --hard")
RESET_UNPUSHED_RE = phrase_re("`git rev-list <last_head_sha>..HEAD` to be empty")
RESET_CHECKOUT_RE = re.compile(
    r"(`git checkout <branch>`|checkout[^.]{0,40}?branch)", re.IGNORECASE)
REVIEW_RANGE_RE = phrase_re(
    "the review range head is your post-recovery `HEAD`, not `last_head_sha`")


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_rejects_a_precondition_re_gated_on_pending_failure(
        tmp_path, subdir, name):
    # Issue #254's exact bug shape, and the one mutation no positional check
    # can see: the precondition stays ahead of the reset, the enforcement
    # clause stays intact and the reset stays conditioned on a clean tree —
    # only the qualifier flips, and the guard then binds solely in the case
    # where a `failure_report` already announced the interruption. A turn
    # killed hard sends no report, so `pending_failure` is null exactly when
    # the uncommitted work is at risk.
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    text = template.read_text()
    mutated, count = RESET_QUALIFIER_RE.subn(
        "only when `pending_failure` is non-null", text)
    assert count, f"{name}: no unconditional qualifier to mutate"
    template.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: `git status --porcelain` precondition does not bind "
            "unconditionally") in r.stdout


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_rejects_an_unguarded_reset_appended_after_a_guarded_one(
        tmp_path, subdir, name):
    # The conditional has to bind per reset. Searched file-globally, one
    # "only when the worktree is clean" anywhere satisfied it no matter how
    # many bare resets followed — so a fully guarded step 1 plus a trailing
    # unconditional reset linted green.
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    template.write_text(template.read_text() + "\n\nLater: just "
                        "`git reset --hard <last_head_sha>` no matter what.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: 1 of 2 `git reset --hard` instructions are not "
            "conditioned on a clean worktree") in r.stdout


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_requires_the_precondition_to_state_its_consequence(
        tmp_path, subdir, name):
    # A precondition with no stated consequence is a description, not a guard:
    # the worker is told what to require and nothing about what to do when the
    # requirement fails.
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    text = template.read_text()
    mutated, count = RESET_ENFORCEMENT_RE.subn("note the state", text)
    assert count, f"{name}: no enforcement clause to mutate"
    template.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: cleanliness precondition states no consequence — "
            'expected an explicit "do not run `git reset --hard`"') in r.stdout


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_requires_the_unpushed_commit_half_of_the_precondition(
        tmp_path, subdir, name):
    # `--porcelain` reports the working tree and the index and says nothing
    # about commits that were made but never pushed — which `git reset --hard
    # <last_head_sha>` discards just as completely. `iron-build` commits per
    # task, so a turn killed partway through is far more likely to leave
    # committed work than unstaged work: the half of the hazard the original
    # guard did not cover was the likelier half.
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    mutated, count = RESET_UNPUSHED_RE.subn(
        "`git stash list` to be empty", template.read_text())
    assert count, f"{name}: no unpushed-commit clause to mutate"
    template.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: cleanliness precondition covers only the working "
            "tree") in r.stdout


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_requires_a_checkout_before_the_reset(tmp_path, subdir, name):
    # `git fetch` + `git reset --hard` never moves the checkout. A turn that
    # inherits whatever branch its predecessor left behind resets THAT branch
    # to the session head and then pushes from it — and every one of these
    # turns commits and pushes. `collab-turn-review-local.md` shipped with no
    # checkout at all.
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    mutated, count = RESET_CHECKOUT_RE.subn("continue", template.read_text())
    assert count, f"{name}: no checkout to mutate"
    template.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: does not check out the session branch before "
            "resetting") in r.stdout


REVIEW_RANGE_PROMPTS = [
    (".claude-plugin/prompts", "collab-turn-review-local.md"),
    (".claude-plugin/prompts", "collab-turn-review-fix-global.md"),
    (".codex-plugin/prompts", "collab-review-local.md"),
    (".codex-plugin/prompts", "collab-global-review.md"),
]


@pytest.mark.parametrize("subdir,name", REVIEW_RANGE_PROMPTS)
def test_lint_requires_recovered_commits_inside_the_reviewed_range(
        tmp_path, subdir, name):
    # These turns review `base_sha..last_head_sha` read from status but send
    # their CURRENT head. A recovery owner commits what it recovered, so its
    # head moves past the recorded one — and reviewing the recorded range
    # while sending a head beyond it promotes those commits to session head
    # with nobody having read them.
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    mutated, count = REVIEW_RANGE_RE.subn(
        "use the recorded range", template.read_text())
    assert count, f"{name}: no review-range rule to mutate"
    template.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: recovered commits are not brought into the reviewed "
            "range") in r.stdout


def test_lint_requires_the_batch_impl_fast_path_to_check_cleanliness(tmp_path):
    # The fast path returns before the reset, so `check_reset_guards` never
    # reaches it — and the state it skips ahead from is exactly the post-OOM
    # one: HEAD still at `last_head_sha`, branch correct, tree dirty. Nothing
    # is destroyed, but the batch is then built on top of the dead turn's
    # unrecovered work and pushes it as its own.
    fixture = copy_fixture(tmp_path)
    template = (fixture / ".codex-plugin" / "prompts" /
                "collab-batch-impl.md")
    text = template.read_text()
    condition = phrase_re("the checked-out branch equals the session branch, "
                          "and `git status --porcelain` is empty.")
    mutated, count = condition.subn(
        "the checked-out branch equals the session branch.", text)
    assert count, "fast-path condition not found to mutate"
    template.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-batch-impl.md: the fast path is not conditioned on a "
            "clean worktree") in r.stdout


def test_lint_rejects_a_surface_whose_only_resets_are_exempt_mentions(tmp_path):
    # `RESET_MENTION_EXEMPT_RE` excuses the guard's own two references to the
    # reset — the prohibition and the recovery owner's skip list — from the
    # ordering rules. Without this test, widening that exemption until it
    # swallowed the real instruction too would leave the whole per-file check
    # passing vacuously: no reset instruction found means nothing to order the
    # guard against.
    fixture = copy_fixture(tmp_path)
    template = (fixture / ".codex-plugin" / "prompts" /
                "collab-global-review.md")
    text = template.read_text()
    instruction = "Only when the worktree is clean, `git reset --hard <last_head_sha>`."
    assert instruction in text, "reset instruction not found to remove"
    template.write_text(text.replace(
        instruction, "Only when the worktree is clean, sync the branch."))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-global-review.md: expected a `git reset --hard` "
            "instruction to guard") in r.stdout


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_requires_the_recovery_owner_to_be_told_to_skip_the_sync(
        tmp_path, subdir, name):
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    text = template.read_text()
    for marker in ("skip the sync", "skip this step entirely", "Skip only the"):
        if marker in text:
            template.write_text(text.replace(marker, "proceed with the sync"))
            break
    else:
        pytest.fail(f"{name}: no recovery-skip marker to mutate")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{name}: recovery owner is not told to skip the sync" in r.stdout


@pytest.mark.parametrize("subdir,name", RESET_GUARD_PROMPTS)
def test_lint_requires_the_orphan_recovery_to_be_reported(
        tmp_path, subdir, name):
    # Preserving the orphaned work is necessary but not sufficient. A turn
    # that recovers it and then completes normally leaves status, event
    # history and the human all seeing an ordinary turn — the lost turn is
    # invisible. Every prompt that guards a reset must also record the
    # incident with a non-advancing `orphan_recovered` send.
    fixture = copy_fixture(tmp_path)
    template = fixture / subdir / name
    text = template.read_text()
    assert 'topic="orphan_recovered"' in text, f"{name}: no report to mutate"
    template.write_text(text.replace('topic="orphan_recovered"',
                                     'topic="review_local"'))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{name}: a dirty worktree on a normal turn is never reported") \
        in r.stdout


def test_lint_rejects_a_precondition_stated_after_the_reset(tmp_path):
    # Substring membership has no ordering. A precondition that appears after
    # the reset it governs is not a precondition, and a pure-substring gate
    # cannot tell the difference.
    fixture = copy_fixture(tmp_path)
    template = (fixture / ".claude-plugin" / "prompts" /
                "collab-turn-review-local.md")
    text = template.read_text()
    guard_start = text.index("Immediately before resetting,")
    guard_end = text.index("**Both normal and recovery turns**")
    guard = text[guard_start:guard_end]
    # Move the whole guard paragraph to after the step that resets.
    moved = (text[:guard_start] + "`git reset --hard <last_head_sha>`.\n   "
             + text[guard_end:].replace(
                 "2. Prepare the normal review input",
                 f"2. Afterwards: {guard}\n3. Prepare the normal review input", 1))
    template.write_text(moved)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-review-local.md: `git status --porcelain` "
            "precondition appears after the `git reset --hard` it must guard"
            ) in r.stdout


def test_lint_rejects_the_whole_guard_demoted_below_the_reset(tmp_path):
    # The enforcement, recovery-skip and conditional clauses used to be
    # searched file-globally, so restoring the naive step 1 and re-filing the
    # entire guard as a trailing "Historical note" left three of the four
    # checks green — the guard was still *in* the file, just nowhere the
    # worker reads before resetting. All four must object.
    fixture = copy_fixture(tmp_path)
    template = (fixture / ".claude-plugin" / "prompts" /
                "collab-turn-review-local.md")
    text = template.read_text()
    start = text.index("## Actions\n1. **Normal turns only")
    end = text.index("\n2. Prepare the normal review input")
    guard = text[start:end]
    naive = ("## Actions\n"
             "1. Pre-send harness: `git fetch`; "
             "`git cat-file -e <last_head_sha>^{commit}`\n"
             "   (on miss → `failure_report` `branch_drift:...`);\n"
             "   `git reset --hard <last_head_sha>`; `cargo fmt --check`.")
    template.write_text(text[:start] + naive + text[end:]
                        + "\n\n## Historical note\n\n" + guard + "\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    name = "collab-turn-review-local.md"
    for diagnostic in (
        f"{name}: `git status --porcelain` precondition appears after the "
        "`git reset --hard` it must guard",
        f"{name}: the enforcement clause appears after the "
        "`git reset --hard` it must govern",
        f"{name}: the recovery skip clause appears after the "
        "`git reset --hard` it must govern",
        f"{name}: 1 of 2 `git reset --hard` instructions are not conditioned "
        "on a clean worktree",
    ):
        assert diagnostic in r.stdout, diagnostic


def test_lint_rejects_a_precondition_that_is_stated_then_overridden(tmp_path):
    # The guard can be present and immediately negated. Requiring the reset to
    # be conditioned on a clean tree is what catches this; asserting the
    # precondition's words appear does not.
    fixture = copy_fixture(tmp_path)
    template = (fixture / ".claude-plugin" / "prompts" /
                "collab-turn-review-local.md")
    text = template.read_text()
    mutated, count = RESET_CONDITIONAL_RE.subn(
        "If dirty, note it and continue anyway; `git reset --hard", text)
    assert count, "conditional reset not found to mutate"
    template.write_text(mutated)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-review-local.md: 1 of 1 `git reset --hard` "
            "instructions are not conditioned on a clean worktree") in r.stdout


def test_lint_rejects_issue_254_pre_fix_text_with_the_guard_parked_in_a_comment(
        tmp_path):
    # The regression that motivated the structural check. Restoring issue
    # #254's unconditional reset while parking the guard's phrases in an HTML
    # comment satisfies every substring pin — verified green against the
    # substring-only gate this replaced.
    fixture = copy_fixture(tmp_path)
    template = (fixture / ".claude-plugin" / "prompts" /
                "collab-turn-review-local.md")
    text = template.read_text()
    start = text.index("## Actions\n1. **Normal turns only")
    end = text.index("\n2. Prepare the normal review input")
    pre_fix = (
        "## Actions\n"
        "1. Pre-send harness: `git fetch`; `git cat-file -e <last_head_sha>^{commit}`\n"
        "   (on miss → `failure_report` `branch_drift:...`);\n"
        "   `git reset --hard <last_head_sha>`; `cargo fmt --check`.\n"
        "<!-- guard phrases parked out of the executable path:\n"
        "`git status --porcelain` to be empty regardless of `pending_failure`: a\n"
        "do not run `git reset --hard`; instead preserve\n"
        "Only when the worktree is clean\n"
        "as recovery owner, skip the sync\n"
        "-->")
    template.write_text(text[:start] + pre_fix + text[end:])

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-review-local.md: no `git status --porcelain` "
            "cleanliness precondition guarding `git reset --hard`") in r.stdout


def test_lint_requires_retry_safe_split_contract(tmp_path):
    fixture = copy_fixture(tmp_path)
    prompt = fixture / ".claude-plugin" / "commands" / "evaluate-issue.md"
    prompt.write_text(
        prompt.read_text().replace("Split-child-key:", "Child split key:", 1)
    )

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ".claude-plugin/commands/evaluate-issue.md: missing evaluate-issue SPLIT contract" \
        in r.stdout


def test_lint_catches_reverted_submit_sender_claim(tmp_path):
    # Task 5 regression pin: collab-turn-submit.md's `final` send site
    # reverting `sender="$SENDER"` back to a hardcoded `sender="claude"`
    # bypasses the post-gate $SENDER authorization check entirely.
    fixture = copy_fixture(tmp_path)
    bad = fixture / ".claude-plugin" / "prompts" / "collab-turn-submit.md"
    text = bad.read_text()
    target = 'collab_send(sender="$SENDER", topic="final",'
    assert target in text, "final send call site not found to mutate"
    bad.write_text(text.replace(target, 'collab_send(sender="claude", topic="final",', 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-submit.md: forbidden stale direct-body claim "
            "'sender=\"claude\"'") in r.stdout


def test_lint_allows_sender_placeholder_but_still_rejects_unknown_ones(tmp_path):
    # Task 1 added $SENDER to ALLOWED_PLACEHOLDERS. Prove that didn't
    # accidentally disable the unknown-placeholder check altogether: the
    # unmodified fixture (which uses $SENDER throughout
    # collab-turn-submit.md) must lint clean, and a genuinely unrecognized
    # placeholder introduced alongside it must still fail.
    fixture = copy_fixture(tmp_path)
    submit = fixture / ".claude-plugin" / "prompts" / "collab-turn-submit.md"
    text = submit.read_text()
    assert "$SENDER" in text

    # `test_fixture_is_green` covers the clean-exit case for the whole
    # fixture; this test stays narrow on purpose, asserting only that the
    # placeholder check specifically never flags $SENDER — the thing Task 1's
    # allowlist entry controls.
    r_baseline = run({"COLLAB_LINT_ROOT": str(fixture)})
    assert "unknown placeholder $SENDER" not in r_baseline.stdout

    submit.write_text(text + "\nUse $STILL_NOT_ALLOWED here.\n")
    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "unknown placeholder $STILL_NOT_ALLOWED" in r.stdout


def test_lint_catches_dispatch_row_missing_current_owner_source(tmp_path):
    # Task 6 regression pin: the PlanClaudeFinalizePending dispatch row must
    # name `$SENDER=<collab_status.current_owner>` as the $SENDER
    # substitution source. Weakening it to a vaguer `<owner>` placeholder
    # must fail closed rather than lint green.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = cmd.read_text()
    lines = text.splitlines()
    old_row = next(l for l in lines if l.startswith("| `PlanClaudeFinalizePending` |"))
    assert "$SENDER=<collab_status.current_owner>" in old_row
    new_row = old_row.replace("$SENDER=<collab_status.current_owner>", "$SENDER=<owner>", 1)
    cmd.write_text(text.replace(old_row, new_row, 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab.md: PlanClaudeFinalizePending row must derive $SENDER "
            "from `$SENDER=<collab_status.current_owner>` (current_owner "
            "read from collab_status)") in r.stdout


def test_lint_rejects_a_pilot_only_sender_derivation_in_dispatch_row(tmp_path):
    # Mutates the CodeReviewFinalPending row's $SENDER derivation to a
    # pilot-only form and asserts the lint's diagnostic for it. It proves the
    # gate, not the routing itself: nothing here runs a session or observes a
    # send. If the row regresses this way, the recovery-owner substitution
    # stops applying and a recovery completion under a non-pilot current_owner
    # dispatches the wrong sender identity — PILOT_ONLY_SENDER_RE must catch
    # it.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = cmd.read_text()
    lines = text.splitlines()
    old_row = next(l for l in lines if l.startswith("| `CodeReviewFinalPending` |"))
    assert "$SENDER=<collab_status.current_owner>" in old_row
    new_row = old_row.replace("$SENDER=<collab_status.current_owner>", "$SENDER=<pilot>", 1)
    cmd.write_text(text.replace(old_row, new_row, 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab.md: CodeReviewFinalPending row must not derive $SENDER "
            "directly from pilot — $SENDER must come from current_owner") \
        in r.stdout


def test_lint_catches_dispatch_row_with_recovery_owner_clause_removed(tmp_path):
    # Task 6 also requires each row to name the recovery-owner case, so the
    # new guard can't itself strand an existing recovery completion by
    # silently accepting a row that dropped that case. Rewriting the
    # CodeReviewFinalPending row's "the recovery owner" mention to neutral
    # prose (while leaving the current_owner substitution intact) must still
    # fail.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = cmd.read_text()
    lines = text.splitlines()
    old_row = next(l for l in lines if l.startswith("| `CodeReviewFinalPending` |"))
    target = "may instead be the recovery owner per the recovery override"
    assert target in old_row, "recovery-owner clause not found to mutate"
    new_row = old_row.replace(
        target, "may instead be an alternate agent per the recovery override", 1)
    assert "$SENDER=<collab_status.current_owner>" in new_row, \
        "mutation must not disturb the current_owner substitution"
    cmd.write_text(text.replace(old_row, new_row, 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "collab.md: CodeReviewFinalPending row must name the recovery-owner case" \
        in r.stdout


def test_lint_catches_a_dispatch_row_that_negates_the_live_recovery_case(tmp_path):
    # The negation the phrase-search could not distinguish from the real
    # thing: `CodeReviewFinalPending` IS `is_coding_active()`, so the
    # recovery-owner substitution is live for it. A row rewritten to say the
    # substitution does not apply keeps every "recovery owner" word on the
    # line and keeps the `$SENDER=<collab_status.current_owner>` pin intact.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = cmd.read_text()
    old_row = next(l for l in text.splitlines()
                   if l.startswith("| `CodeReviewFinalPending` |")
                   and "collab-turn-submit.md" in l)
    new_row = old_row.replace(
        "`current_owner` may instead be the recovery owner per the recovery override",
        "the recovery-owner substitution does **not** apply to this phase; "
        "`current_owner`", 1)
    assert new_row != old_row, "recovery-owner clause not found to mutate"
    assert "$SENDER=<collab_status.current_owner>" in new_row
    assert re.search(r"recovery[-\s]owner", new_row), \
        "the mutation must keep the phrase — that is all the old check saw"
    cmd.write_text(text.replace(old_row, new_row, 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab.md: CodeReviewFinalPending row must not disclaim the "
            "recovery-owner substitution") in r.stdout


def test_lint_catches_a_dispatch_row_lookup_pointing_at_the_wrong_table(tmp_path):
    # Two lines start with each audited row prefix — the phase-action row and
    # the Codex tuning-matrix row. The audit must assert how many it expects
    # and pick its row by a second column, not take whichever comes first.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = cmd.read_text()
    matrix_row = next(l for l in text.splitlines()
                      if l.startswith("| `PlanClaudeFinalizePending` |")
                      and "collab-plan-finalize.md" in l)
    cmd.write_text(text.replace(matrix_row + "\n", ""))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab.md: expected exactly 2 lines starting with "
            "'| `PlanClaudeFinalizePending` |', found 1") in r.stdout


@pytest.mark.parametrize("snippet", TASK_LIST_BRIDGE_SNIPPETS)
def test_lint_requires_every_task_list_bridge_sender_pin(tmp_path, snippet):
    # ITEM 7: the PlanLocked bridge passes `$SENDER` to a worker that must
    # send as the pilot (`codex` under pilot=codex), and the bridge is a
    # numbered list item, not a table row, so the row helper never saw it.
    fixture = copy_fixture(tmp_path)
    mutate(fixture / ".claude-plugin" / "commands" / "collab.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".claude-plugin/commands/collab.md: missing PlanLocked task-list "
            f"bridge sender contract {snippet!r}") in r.stdout


@pytest.mark.parametrize("snippet", DISPATCH_FAILURE_ADMISSIBILITY_SNIPPETS)
def test_lint_requires_dispatch_failure_admissibility_guard(tmp_path, snippet):
    # Routing Codex into new phases makes the wait loop's
    # `codex_dispatch_failed:` remedy reachable in phases the server rejects it
    # in. TWO gates apply and passing the first is not enough: the planning
    # phases fail `Phase::is_coding_active()`, while `CodeReviewLocalPending`
    # and `CodeReviewFinalPending` pass that check yet are still refused by
    # `dispatch_failure_phase_admits` (it admits only `CodeImplementPending`
    # with implementer=codex, and `CodeReviewFixGlobalPending`). Reasoning from
    # `is_coding_active()` alone is what previously put those two phases in the
    # "send it" bucket; under `pilot == "codex"` that sends the dispatcher into
    # a rejected send with no reachable exit.
    fixture = copy_fixture(tmp_path)
    mutate(fixture / ".claude-plugin" / "commands" / "collab.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".claude-plugin/commands/collab.md: missing dispatch-failure "
            f"admissibility contract {snippet!r}") in r.stdout


@pytest.mark.parametrize("snippet", CODEX_PILOT_ROUTING_SNIPPETS)
def test_lint_catches_missing_codex_pilot_compose_route(tmp_path, snippet):
    # Every entry, not one arbitrary member: the surviving members are the
    # contract data, and that is what regresses.
    fixture = copy_fixture(tmp_path)
    mutate(fixture / ".codex-plugin" / "commands" / "collab.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".codex-plugin/commands/collab.md: missing Codex-pilot routing "
            f"contract {snippet!r}") in r.stdout


def test_lint_catches_a_missing_codex_command_file(tmp_path):
    # The `not CODEX_COMMAND.exists()` branch, which no fixture reached.
    fixture = copy_fixture(tmp_path)
    shutil.rmtree(fixture / ".codex-plugin" / "commands")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".codex-plugin/commands/collab.md: missing Codex-pilot routing "
            "contract") in r.stdout


@pytest.mark.parametrize("snippet", COMPOSE_HANDOFF_SNIPPETS)
def test_lint_catches_missing_codex_pilot_compose_handoff(tmp_path, snippet):
    # Includes the two bullet-bound `$SENDER` pins. The bare literal
    # `$SENDER=<collab_status.current_owner>` occurs six times in collab.md,
    # so rewriting either bullet to `$SENDER=<pilot>`, to `$SENDER=claude`, or
    # deleting its clause used to leave this check green.
    fixture = copy_fixture(tmp_path)
    mutate(fixture / ".claude-plugin" / "commands" / "collab.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".claude-plugin/commands/collab.md: missing Codex-pilot compose "
            f"handoff contract {snippet!r}") in r.stdout


@pytest.mark.parametrize("replacement", ["<pilot>", "claude"])
@pytest.mark.parametrize("topic", ["final", "final_review"])
def test_lint_catches_a_compose_handoff_bullet_losing_current_owner(
        tmp_path, topic, replacement):
    # The regression shape itself, not just the literal-pin deletion: each
    # compose-handoff bullet's own `$SENDER` clause rewritten in place, with
    # the dispatch-table rows left untouched.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    bullet = next(s for s in COMPOSE_HANDOFF_SNIPPETS
                  if s.startswith(f"`$TOPIC={topic}`"))
    mutate(cmd, bullet,
           bullet.replace("<collab_status.current_owner>", replacement))
    assert "`$SENDER=<collab_status.current_owner>`" in cmd.read_text(), \
        "the dispatch-table rows must stay intact — they are the decoys"

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".claude-plugin/commands/collab.md: missing Codex-pilot compose "
            f"handoff contract {bullet!r}") in r.stdout


def test_lint_rejects_a_pilot_only_sender_derivation_anywhere_in_collab_md(tmp_path):
    # PILOT_ONLY_SENDER_RE used to run only over the two matched dispatch
    # rows, so a pilot-only derivation anywhere else in the file — the
    # tuning matrix, the compose bullets, the bridge — was invisible.
    fixture = copy_fixture(tmp_path)
    cmd = fixture / ".claude-plugin" / "commands" / "collab.md"
    row = next(l for l in cmd.read_text().splitlines()
               if l.startswith("| `CodeReviewFinalPending` |")
               and "collab-final-review.md" in l)
    mutate(cmd, row, row + "\n<!-- $SENDER=<pilot> -->")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "$SENDER must never be derived directly from `pilot`" in r.stdout


@pytest.mark.parametrize("snippet", DOC_PR_BASE_SNIPPETS)
def test_lint_catches_stale_docs_pr_base_containment_rule(tmp_path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate(fixture / "docs" / "COLLAB.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"docs/COLLAB.md: missing PR-base resolution contract {snippet!r}") \
        in r.stdout


@pytest.mark.parametrize("snippet", DOC_PILOT_SUBMIT_SNIPPETS)
def test_lint_catches_missing_docs_pilot_submit_contract(tmp_path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate(fixture / "docs" / "COLLAB.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"docs/COLLAB.md: missing pilot-submit routing contract {snippet!r}") \
        in r.stdout


@pytest.mark.parametrize("snippet", SUBMIT_TEMPLATE_SNIPPETS)
def test_lint_requires_every_submit_template_pin(tmp_path, snippet):
    # The whole REQUIRED_TEMPLATE_SNIPPETS["collab-turn-submit.md"] block —
    # all four call-site pins and all four guard-prose pins — could be
    # deleted with the suite still fully green. Each entry is now proven.
    fixture = copy_fixture(tmp_path)
    mutate(fixture / ".claude-plugin" / "prompts" / "collab-turn-submit.md",
           snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"collab-turn-submit.md: missing required contract snippet "
            f"{snippet!r}") in r.stdout


@pytest.mark.parametrize("snippet", TASK_LIST_TEMPLATE_SNIPPETS)
def test_lint_requires_every_task_list_template_pin(tmp_path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate(fixture / ".claude-plugin" / "prompts" / "collab-turn-task-list.md",
           snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"collab-turn-task-list.md: missing required contract snippet "
            f"{snippet!r}") in r.stdout


def test_lint_catches_a_senderless_failure_report_send(tmp_path):
    # ITEM 3: the `pr_create_failed` site with `sender=` dropped entirely is
    # neither a `sender="claude"` literal nor a missing `topic=` — the pins
    # that started at `topic="failure_report",` asserted nothing about the
    # sender, leaving the Rust count assertion as the only gate on the one
    # site commit 13f38b6 records as "previously missed".
    fixture = copy_fixture(tmp_path)
    submit = fixture / ".claude-plugin" / "prompts" / "collab-turn-submit.md"
    mutate(submit,
           'base-branch resolution failure, `collab_send(sender="$SENDER",\n'
           '  topic="failure_report",',
           'base-branch resolution failure, `collab_send(\n'
           '  topic="failure_report",')

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ('collab-turn-submit.md: missing required contract snippet '
            '\'collab_send(sender="$SENDER",\\n  topic="failure_report",\\n'
            '  content=<JSON {"coding_failure":"pr_create_failed:\'') in r.stdout


def test_lint_catches_the_submit_abort_directive_becoming_a_fallback(tmp_path):
    # ITEM 2: the guard's rationale pins all keep matching when its
    # ENFORCEMENT clause is rewritten from a hard abort into exactly the
    # identity fallback this branch removed.
    fixture = copy_fixture(tmp_path)
    submit = fixture / ".claude-plugin" / "prompts" / "collab-turn-submit.md"
    mutate(submit,
           "equal `current_owner`, ABORT — do not send anything — and report the\n"
           "   mismatch on the verdict's blocker line.",
           "equal `current_owner`, fall back to `current_owner` and continue\n"
           "   with the send.")
    text = submit.read_text()
    for surviving in ("Verify `$SENDER` against `collab_status.current_owner`",
                      "MUST NOT be\n   substituted with your own identity",
                      "may\n     legitimately be the recovery owner rather than the pilot"):
        assert surviving in text, \
            "the rationale pins must survive — they are why this mutation used to pass"
    assert text.count('sender="$SENDER"') == 4, \
        "the Rust call-site count must survive too"

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-submit.md: missing required contract snippet 'equal "
            "`current_owner`, ABORT — do not send anything — and report the'") \
        in r.stdout


@pytest.mark.parametrize("hardcoded", ['sender="claude"', "sender='claude'",
                                       "sender=claude", 'sender="Claude"'])
@pytest.mark.parametrize("template", ["collab-turn-submit.md",
                                      "collab-turn-task-list.md"])
def test_lint_rejects_any_hardcoded_claude_sender(tmp_path, template, hardcoded):
    # The literal-only FORBIDDEN pin let `sender='claude'`, `sender=claude`
    # and `sender="Claude"` through — all the same bug.
    fixture = copy_fixture(tmp_path)
    path = fixture / ".claude-plugin" / "prompts" / template
    path.write_text(path.read_text() + f"\n`collab_send({hardcoded}, ...)`\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{template}: hardcoded sender identity" in r.stdout


def test_lint_catches_the_task_list_send_reverting_to_claude(tmp_path):
    # ITEM 7's headline regression: PlanLocked is entered with
    # `current_owner == codex` under pilot=codex, and is not
    # `is_coding_active()`, so a rejected send has no failure_report escape.
    fixture = copy_fixture(tmp_path)
    task_list = (fixture / ".claude-plugin" / "prompts" /
                 "collab-turn-task-list.md")
    mutate(task_list, 'collab_send(sender="$SENDER", topic="task_list",',
           'collab_send(sender="claude", topic="task_list",')

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ('collab-turn-task-list.md: missing required contract snippet '
            '\'collab_send(sender="$SENDER", topic="task_list",\'') in r.stdout
    assert ('collab-turn-task-list.md: forbidden stale direct-body claim '
            '\'sender="claude"\'') in r.stdout


# ── The gate pins must read the *executable* prose, not the raw file ────────
#
# Every check above asserts a rule is stated where an agent will read it, and
# an agent reads the rendered body — not a commented-out "historical note".
# A raw-text substring search cannot tell the two apart, so a pin was once
# satisfiable by its own epitaph: park the live rule in an HTML comment,
# write the opposite instruction underneath, and the lint stayed green while
# the shipped instruction said the reverse. These tests are the proof that
# the comment-stripping in `live_text()` is wired into the gate checks; both
# mutations below were verified green against the pre-fix lint.

def comment_out(path, start, end, replacement):
    """Park `start`..`end` in an HTML comment and substitute `replacement`.

    Models the realistic edit — provenance kept, instruction replaced — not a
    deletion. A deletion reds any substring pin; only this shape distinguishes
    a pin that reads the live body from one that reads the raw bytes.
    """
    text = path.read_text()
    i = text.index(start)
    j = text.index(end, i)
    path.write_text(
        f"{text[:i]}<!-- HISTORICAL NOTE:\n{text[i:j]}\n-->\n{replacement}{text[j:]}")


def test_lint_rejects_the_approval_gate_demoted_into_an_html_comment(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    comment_out(
        command,
        "0. **Dispatcher-owned planning approval gate.**",
        "1. Read `current_owner` from `collab_status`",
        "0. Dispatch the task list immediately; no user approval is required.\n\n",
    )

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("v3 bridge is missing dispatcher approval-gate contract "
            "'**Dispatcher-owned planning approval gate.**'") in r.stdout


def test_lint_rejects_the_no_silent_fallback_rule_demoted_into_a_comment(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    comment_out(
        command,
        "**Malformed flag input is a hard usage error",
        "Stop on the error",
        "On any unrecognized `--pilot` value, quietly use the default "
        "`claude` and continue. ",
    )

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("`start` section is missing pilot-flag parsing contract "
            "'do not silently fall back to the default on a malformed flag'") in r.stdout


def test_lint_rejects_the_approval_gate_moved_below_the_dispatch_it_guards(tmp_path):
    # Presence is not enough: a gate stated *after* the dispatch it guards is
    # documentation, not a gate — the bridge would send `task_list` and only
    # then ask. Every phrase pin still passes under this mutation, so only the
    # ordering assertion can catch it.
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = command.read_text()
    start = text.index("0. **Dispatcher-owned planning approval gate.**")
    end = text.index("1. Read `current_owner` from `collab_status`", start)
    gate, rest = text[start:end], text[end:]
    after_dispatch = rest.index(
        "2. The worker must reject the bridge before sending if:")
    command.write_text(
        text[:start] + rest[:after_dispatch] + gate + rest[after_dispatch:])

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "must appear BEFORE the bridge dispatches" in r.stdout


# ── The gate must be REACHABLE and ATTENDED, not merely stated ──────────────
#
# The pins above prove the gate is written down, before the dispatch it
# guards. Two ways it can still never gate anything: the loop exits at
# `PlanLocked` (it is in the v1 terminal set) before reaching the bridge, or
# the turn is handed to an unattended `claude -p` successor with no human to
# ask. Both were harmless under the old placement — the gate had already
# fired one phase earlier — which is why neither was pinned.

PLAN_LOCKED_REACHABILITY_SNIPPETS = [
    "if phase == PlanLocked and no task_list has been sent yet:",
    "enter § v3 Bridge, step 0 (the approval gate) — do NOT exit the loop",
    "`PlanLocked` is terminal for `wait_my_turn`, not for the dispatch loop.",
    "routes there instead of exiting",
    "Never spawn an unattended successor into the planning gate.",
]


@pytest.mark.parametrize("snippet", PLAN_LOCKED_REACHABILITY_SNIPPETS)
def test_lint_requires_the_approval_gate_to_stay_reachable(tmp_path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / ".claude-plugin" / "commands" / "collab.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "must route `PlanLocked` pre-`task_list` into the approval gate" in r.stdout


# ── Reachability is an ORDER, not a presence ────────────────────────────────
#
# Every phrase above can be satisfied by the same pseudocode block moved from
# above the terminal-set branch to below it: no text is added or removed, so
# each pin still matches — and `PlanLocked` is in the v1 terminal set, so the
# loop matches the exit first, logs `t10_session_complete` and ends the session
# with `final_plan_hash` set and the human never asked. That is the exact
# regression the block's own comment says it exists to prevent, and it was
# verified GREEN against the pre-fix lint before this test existed.

def test_lint_rejects_the_planlocked_branch_moved_below_the_terminal_exit(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    text = command.read_text()
    start = text.index(
        "  # PlanLocked pre-`task_list` is in the v1 terminal set")
    end = text.index("  if session_ended or phase in terminal_set:", start)
    block, rest = text[start:end], text[end:]
    after_exit = rest.index('  if current_owner == "codex":')
    command.write_text(
        text[:start] + rest[:after_exit] + block + rest[after_exit:])

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "must be tested BEFORE" in r.stdout
    assert "a branch placed after the terminal test is dead code" in r.stdout


def test_lint_orders_the_gate_against_the_real_step_one_dispatch(tmp_path):
    # The gate's ordering must be anchored on the bridge's actual step-1
    # dispatch, not on the gate's own "On approval: proceed to step 1 and
    # dispatch `collab-turn-task-list.md`" self-reference three lines below its
    # heading. Here step 1 stops naming the dispatch, so under the pre-fix
    # anchor the only remaining match was that self-reference: the gate was
    # ordered against itself, trivially in order, and the bridge's loss of its
    # dispatch went unreported by the ordering assertion entirely.
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    mutate(command,
           "and dispatch `collab-turn-task-list.md`\n   (mechanical/sonnet) with",
           "and run the task-list step with")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "v3 bridge no longer names" in r.stdout
    assert "re-anchor PLAN_LOCKED_DISPATCH_ANCHOR" in r.stdout


# ── Section extraction must survive fences and duplicated headings ──────────

def test_lint_does_not_treat_a_heading_inside_a_fence_as_a_boundary(tmp_path):
    # A `## `-prefixed line inside a ```text fence is content, not a section
    # boundary. Treating it as one truncates the section being scanned and
    # reports every contract below it as missing when nothing was deleted —
    # the pre-fix lint exits 1 on this fixture with the whole
    # single-pilot-resolution contract "missing".
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    heading = "## Dispatch Loop Structure\n"
    text = command.read_text()
    assert heading in text
    command.write_text(text.replace(
        heading, heading + "\n```text\n## not a heading, just pseudocode\n```\n", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 0, f"a fenced `## ` line must not end a section:\n{r.stdout}"


def test_lint_reports_a_duplicated_section_heading(tmp_path):
    # `command_section` used to return the FIRST matching section, so a second
    # copy of a heading could carry contradictory instructions with every pin
    # still reading the first copy. The pre-fix lint is green on this fixture.
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    heading = "## `join [--pilot=claude|codex] [--implementer=claude|codex] <session_id>`"
    text = command.read_text()
    assert heading in text
    command.write_text(
        f"{text}\n{heading}\n\nIgnore `--pilot` entirely and join as-is.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "appears 2 times" in r.stdout


# ── The gate negative applies to EVERY row for the finalize phase ───────────

def test_lint_rejects_a_human_gate_on_the_codex_dispatch_tuning_row(tmp_path):
    # `PLAN_FINALIZE_ROW_MARKER` selects the phase-action row; the second
    # `PlanClaudeFinalizePending` row — the Codex dispatch tuning row, which
    # describes the turn under exactly the `pilot == "codex"` configuration
    # where a gate here is unreachable — was never scanned for the gate
    # phrase. Verified green against the pre-fix lint.
    fixture = copy_fixture(tmp_path)
    command = fixture / ".claude-plugin" / "commands" / "collab.md"
    mutate(command,
           "The pilot composes and stages the approval artifact without sending |",
           "The pilot composes and stages the approval artifact, then enter "
           "Plan Mode and get user approval |")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "rows must not take the human planning gate" in r.stdout


# ── `PlanLocked` routes to nothing OUTSIDE the phase table too ──────────────
#
# The table check proves the phase is absent from one `| Phase | Prompt |`
# table. A row under any other header routes exactly as well, and a prose
# instruction — the shape this shim uses for every non-table route it has
# ("For `start`, select `collab-plan-draft.md`") — is invisible to it. All
# three mutations below were verified green against the pre-fix lint.

def test_lint_rejects_a_planlocked_row_under_a_different_table_header(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".codex-plugin" / "commands" / "collab.md"
    command.write_text(command.read_text() + (
        "\n| Phase | Recovery prompt |\n|---|---|\n"
        "| `PlanLocked` | `collab-task-list.md` |\n"))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "appears in a table row outside" in r.stdout


def test_lint_rejects_a_prose_planlocked_routing_instruction(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".codex-plugin" / "commands" / "collab.md"
    command.write_text(command.read_text() +
                       "\nFor `PlanLocked`, select `collab-task-list.md`.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "is a routing instruction for `collab-task-list.md`" in r.stdout


def test_lint_requires_the_shim_to_keep_saying_never_select_the_task_list(tmp_path):
    fixture = copy_fixture(tmp_path)
    command = fixture / ".codex-plugin" / "commands" / "collab.md"
    mutate_flex(command, "Never select `collab-task-list.md`.",
                "Select `collab-task-list.md` in that case.")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "missing the sentence that keeps `collab-task-list.md` unrouted" in r.stdout
    assert "is a routing instruction for `collab-task-list.md`" in r.stdout


# ── The $SENDER and Codex turn-boundary pins read the live body ─────────────
#
# `live_text()` was wired into the pilot/gate checks only. The pins guarding
# the `$SENDER` sender-authorization contract and the Codex turn boundaries
# still matched raw bytes, so the HTML-comment demotion those checks exist to
# close was still open for them. Both mutations below were verified green
# against the pre-fix lint.

def test_lint_rejects_the_sender_authorization_guard_demoted_into_a_comment(tmp_path):
    fixture = copy_fixture(tmp_path)
    submit = fixture / ".claude-plugin" / "prompts" / "collab-turn-submit.md"
    comment_out(
        submit,
        "2. Verify `$SENDER` against `collab_status.current_owner`",
        "3. Fetch the artifact named by `$ARTIFACT_REF`",
        "2. The sender is always the dispatcher; no verification is needed.\n\n",
    )

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-submit.md: missing required contract snippet "
            "'Verify `$SENDER` against `collab_status.current_owner`'") in r.stdout


def test_lint_rejects_a_codex_turn_boundary_demoted_into_a_comment(tmp_path):
    fixture = copy_fixture(tmp_path)
    finalize = fixture / ".codex-plugin" / "prompts" / "collab-plan-finalize.md"
    comment_out(
        finalize,
        '- Your identity is `"codex"`. **This turn sends nothing.**',
        "- Use IronMEM collab tools;",
        '- Your identity is `"codex"`. Send `final` yourself once the plan is '
        "staged.\n\n",
    )

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-plan-finalize.md: missing required recovery/dispatch "
            "contract '**This turn sends nothing.**'") in r.stdout


# ── A green run must say which tree it was green on ─────────────────────────

def test_lint_success_line_names_the_scanned_root(tmp_path):
    # `COLLAB_LINT_ROOT` redirects every path the lint reads, so a vacuous pass
    # on a fixture, a stale worktree or a half-copied checkout is otherwise
    # indistinguishable from a real one. The pre-fix success line printed only
    # the template and matrix counts.
    fixture = copy_fixture(tmp_path)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 0, r.stdout
    assert str(fixture.resolve()) in r.stdout


# ── Contracts introduced alongside the dispatcher-owned gate ────────────────
#
# Every snippet below is contract DATA duplicated here on purpose (see the
# note above CODEX_PILOT_ROUTING_SNIPPETS): importing the lint's lists would
# make these tests parametrize over "whatever the lint currently pins", so
# deleting an entry would shrink the sweep instead of failing it. Every one of
# these mutations was verified green against the pre-fix lint.

def mutate_quoted_flex(path, snippet, replacement=MARK):
    """`mutate_flex` for a phrase inside a `> ` blockquote.

    Blockquote markers are not whitespace, so a phrase that wraps across two
    quoted lines is unreachable by `mutate_flex` — and, before this fix, by
    the lint's own `flex()` pins. The unattended-successor guard is written as
    a blockquote in both surfaces, so every multi-line pin on it needs this.
    """
    text = path.read_text()
    pattern = r"[\s>]+".join(re.escape(part) for part in snippet.split())
    mutated, count = re.subn(pattern, replacement, text)
    assert count, f"quoted flex target not found in {path.name}: {snippet!r}"
    path.write_text(mutated)


UNATTENDED_SUCCESSOR_SNIPPETS = [
    "Never spawn an unattended successor into the planning gate.",
    "The exclusion is **every v1 planning phase**, not just the gate's own",
    "if `phase` is `PlanParallelDrafts`, `PlanSynthesisPending`, "
    "`PlanCodexReviewPending`, `PlanClaudeFinalizePending`, or `PlanLocked` "
    "with no `task_list` sent",
    "use the **Interactive phases** flow below instead",
    "**Reason the list is this wide, so it cannot be narrowed without "
    "confronting it:**",
    "Dropping a phase from this list is only safe if some other human "
    "checkpoint sits ahead of the gate on that phase's path, and there is "
    "none.",
]
UNATTENDED_SUCCESSOR_SURFACES = [".claude-plugin/commands/collab.md",
                                 "docs/COLLAB.md"]


@pytest.mark.parametrize("path", UNATTENDED_SUCCESSOR_SURFACES)
@pytest.mark.parametrize("snippet", UNATTENDED_SUCCESSOR_SNIPPETS)
def test_lint_requires_the_widened_unattended_successor_guard(
        tmp_path, path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_quoted_flex(fixture / path, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{path}: missing unattended-successor guard {snippet!r}" in r.stdout


BRIDGE_BLOCKER_SURFACES = [
    (".claude-plugin/commands/collab.md",
     "If the worker returns `blocker:`, the bridge is over — report it and "
     "exit the loop.",
     ".claude-plugin/commands/collab.md: v3 bridge is missing "
     "blocker-terminates-the-bridge contract"),
    (".claude-plugin/commands/collab.md", "an unbounded re-approval loop",
     ".claude-plugin/commands/collab.md: v3 bridge is missing "
     "blocker-terminates-the-bridge contract"),
    (".claude-plugin/commands/collab.md",
     "Do not re-dispatch the worker, and do not fall back through the loop "
     "into step 0.",
     ".claude-plugin/commands/collab.md: v3 bridge is missing "
     "blocker-terminates-the-bridge contract"),
    ("docs/COLLAB.md",
     "If the worker returns `blocker:`, the bridge is over — report it and "
     "exit the loop.",
     "docs/COLLAB.md: missing blocker-terminates-the-bridge contract"),
    ("docs/COLLAB.md", "an unbounded re-approval loop",
     "docs/COLLAB.md: missing blocker-terminates-the-bridge contract"),
    ("docs/COLLAB.md",
     "The orchestrator must not re-dispatch the worker or fall back through "
     "the loop into the step-0 gate.",
     "docs/COLLAB.md: missing blocker-terminates-the-bridge contract"),
]


@pytest.mark.parametrize("path,snippet,error", BRIDGE_BLOCKER_SURFACES)
def test_lint_requires_the_bridge_blocker_rule(tmp_path, path, snippet, error):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / path, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{error} {snippet!r}" in r.stdout


DOC_BRIDGE_GATE_SNIPPETS = [
    "The bridge's **parse** is worker-owned; its **dispatch** is gated.",
    "But before it dispatches anything, it takes the **dispatcher-owned "
    "planning approval gate** at step 0",
    "it enters Plan Mode and gets user approval, surfacing only "
    "`{drawer_id, plan_file_path, ≤3-line summary}`",
    "This gate is the dispatcher's and no worker's",
    "On rejection it does not send `task_list` and offers `collab_end` "
    "instead.",
]


@pytest.mark.parametrize("snippet", DOC_BRIDGE_GATE_SNIPPETS)
def test_lint_requires_the_doc_bridge_gate_contract(tmp_path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / "docs" / "COLLAB.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("docs/COLLAB.md: missing v3-bridge approval-gate contract "
            f"{snippet!r}") in r.stdout


CODEX_START_PILOT_REJECTION_SNIPPETS = [
    "`start` takes no `--pilot` flag on this side: reject any `--pilot` token",
    "as a usage error naming the offending token",
    "Never strip it into the task text and never call `collab_start` with a "
    "pilot inferred from it",
]


@pytest.mark.parametrize("snippet", CODEX_START_PILOT_REJECTION_SNIPPETS)
def test_lint_requires_codex_start_to_reject_the_pilot_flag(tmp_path, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / ".codex-plugin" / "commands" / "collab.md", snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".codex-plugin/commands/collab.md: missing `start` `--pilot` "
            f"rejection contract {snippet!r}") in r.stdout


PERMISSION_ALLOWLIST_SURFACES = [".claude-plugin/commands/collab.md",
                                 "docs/COLLAB.md"]


@pytest.mark.parametrize("path", PERMISSION_ALLOWLIST_SURFACES)
def test_lint_rejects_set_pilot_on_the_unattended_permission_allowlist(
        tmp_path, path):
    # The negative is scoped to the allowlist BULLETS: the identifier
    # legitimately appears in the paragraph right below them, in the
    # generation-lease claim list and in the `join` authorization contract, so
    # an unscoped negative would be wrong three times over. Adding it back to
    # the bullets is the edit that has to fail — and did not, pre-fix.
    fixture = copy_fixture(tmp_path)
    mutate(fixture / path, "- Git bash operations",
           "- `mcp__ironmem__collab_set_pilot`\n- Git bash operations")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{path}: `mcp__ironmem__collab_set_pilot` must not be on the "
            "unattended successor's permission allowlist") in r.stdout


@pytest.mark.parametrize("path", PERMISSION_ALLOWLIST_SURFACES)
def test_lint_requires_the_allowlist_to_say_why_set_pilot_is_absent(
        tmp_path, path):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / path,
                "`mcp__ironmem__collab_set_pilot` is deliberately **not** on "
                "this list")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{path}: the permission allowlist must say why "
            "`collab_set_pilot` is absent") in r.stdout


# ---- `--` end-of-options terminator ----------------------------------------
#
# Duplicated from the lint rather than imported, for the reason given at the
# top of this file: importing would make the sweep parametrize over "whatever
# the lint currently pins", so deleting a pin would shrink these tests along
# with it instead of failing them.
#
# Every test below is a CROSS-SURFACE test. The contract is stated once per
# flag-parsing subcommand in three files, and the recurring failure is a
# contract fixed in one surface and left stale in another — so removing it from
# one file at a time is exactly the mutation that has to red, and a sweep that
# only ever removes it everywhere at once would have missed this round's drift.
TERMINATOR_SHARED_SNIPPETS = [
    "**`--` ends the flags.** The first bare `--` token is the end-of-options "
    "terminator: every token after it is literal positional text, never "
    "parsed as a flag and never stripped.",
    "The `--` itself is consumed — it is not part of the captured positional.",
    "Flags are recognized only before the first `--`",
    "anywhere in the token stream before the first `--`",
    "a flag-shaped token after the first `--` is not malformed input — it is "
    "literal positional text, and it must never raise a usage error.",
]
# (surface, every region label the lint must report for that surface).
TERMINATOR_SURFACE_LABELS = [
    (".claude-plugin/commands/collab.md", ["`start`", "`review`", "`join`"]),
    (".codex-plugin/commands/collab.md", ["`start`", "`join`"]),
    ("docs/COLLAB.md", ["§ `/collab` flag parsing"]),
]
TERMINATOR_REGION_SNIPPETS = [
    (".claude-plugin/commands/collab.md", "`start`",
     "**When the task text legitimately contains a flag-shaped token, put "
     "`--` before the task**"),
    (".claude-plugin/commands/collab.md", "`start`",
     "`/collab start -- document how --pilot=codex behaves` records that "
     "whole sentence as the task"),
    (".claude-plugin/commands/collab.md", "`start`",
     "the `--` terminator if one was given, with every token after that `--` "
     "kept verbatim"),
    (".claude-plugin/commands/collab.md", "`review`",
     "**When the short topic legitimately contains a flag-shaped token, put "
     "`--` before the topic**"),
    (".claude-plugin/commands/collab.md", "`review`",
     "`/collab review -- --pilot= handling` reviews that topic verbatim"),
    (".claude-plugin/commands/collab.md", "`join`",
     "**When the session id legitimately contains a flag-shaped token, put "
     "`--` before the id**"),
    (".claude-plugin/commands/collab.md", "`join`",
     "`/collab join -- <session_id>` takes the id verbatim"),
    (".claude-plugin/commands/collab.md", "`join`",
     "both flags, and the `--` terminator if one was given"),
    (".codex-plugin/commands/collab.md", "`start`",
     "That rejection binds only tokens before the first `--`"),
    (".codex-plugin/commands/collab.md", "`start`",
     "`/collab start -- document how --pilot=codex behaves` records that "
     "whole sentence as the task rather than erroring on it"),
    (".codex-plugin/commands/collab.md", "`join`",
     "These rules bind only tokens before the first `--`"),
    (".codex-plugin/commands/collab.md", "`join`",
     "`/collab join -- <session_id>` takes the id verbatim"),
    (".codex-plugin/commands/collab.md", "`join`",
     "leaves `pilot` and `implementer` both untouched"),
    (".codex-plugin/commands/collab.md", "`join`",
     "both flags, and the `--` terminator if one was given, kept verbatim"),
    ("docs/COLLAB.md", "§ `/collab` flag parsing",
     "**Malformed flag input stays a hard usage error**, unchanged by the "
     "terminator"),
    ("docs/COLLAB.md", "§ `/collab` flag parsing",
     "**When the positional text legitimately contains a flag-shaped token, "
     "put `--` before it.**"),
    ("docs/COLLAB.md", "§ `/collab` flag parsing", "[--] <task>"),
    ("docs/COLLAB.md", "§ `/collab` flag parsing", "[--] <session_id>"),
    ("docs/COLLAB.md", "§ `/collab` flag parsing", "[--] <short-topic>"),
]
# The escape hatch — the half of the terminator contract a USER acts on —
# stated once per flag-parsing region in that region's own words about its own
# positional. Listed separately from TERMINATOR_REGION_SNIPPETS above (which it
# partly duplicates) because the assertion is different: this one is swept
# region by region, and every region NOT mutated has to stay green. A pin that
# only proved "the phrase is missing somewhere" would be satisfied by a lint
# that pinned the hatch file-wide, and file-wide is exactly what let the Codex
# shim ship a `start` example with no sentence and a `join` with neither.
#
# All SIX regions, not the four that first carried it. `join`'s positional is a
# session UUID that cannot realistically contain a flag-shaped token, so the
# hatch there is not really about escaping one: it is the promise that
# `/collab join -- <id>`, typed by a user who learned the habit from `start`,
# is accepted rather than rejected as the "extra positional value" that parse
# refuses. Both command files state it for `join`, so both are pinned for it.
ESCAPE_HATCH_REGIONS = [
    (".claude-plugin/commands/collab.md", "`start`",
     "**When the task text legitimately contains a flag-shaped token, put "
     "`--` before the task**"),
    (".claude-plugin/commands/collab.md", "`review`",
     "**When the short topic legitimately contains a flag-shaped token, put "
     "`--` before the topic**"),
    (".claude-plugin/commands/collab.md", "`join`",
     "**When the session id legitimately contains a flag-shaped token, put "
     "`--` before the id**"),
    (".codex-plugin/commands/collab.md", "`start`",
     "**When the task text legitimately contains a flag-shaped token, put "
     "`--` before the task**"),
    (".codex-plugin/commands/collab.md", "`join`",
     "**When the session id legitimately contains a flag-shaped token, put "
     "`--` before the id**"),
    ("docs/COLLAB.md", "§ `/collab` flag parsing",
     "**When the positional text legitimately contains a flag-shaped token, "
     "put `--` before it.**"),
]
DOC_FLAG_PARSING_HEADING = "### `/collab` flag parsing — `--` ends the flags"
TERMINATOR_USAGE_SNIPPETS = [
    (".claude-plugin/commands/collab.md", "frontmatter `description`",
     "/collab start [--pilot=claude|codex] [--implementer=claude|codex] "
     "[--] <task>"),
    (".claude-plugin/commands/collab.md", "frontmatter `description`",
     "/collab review [--pilot=claude|codex] [--] <short-topic>"),
    (".claude-plugin/commands/collab.md", "frontmatter `description`",
     "(`--` ends the flags: everything after it is literal text)"),
    (".claude-plugin/commands/collab.md", "frontmatter `argument-hint`",
     "review [--pilot=claude|codex] [--] <short-topic>"),
    (".claude-plugin/commands/collab.md", "§ `Unknown subcommand`",
     "Usage: /collab start [--pilot=claude|codex] "
     "[--implementer=claude|codex] [--] <task>"),
    (".claude-plugin/commands/collab.md", "§ `Unknown subcommand`",
     "`--` ends the flags: every token after it is literal text, so put `--` "
     "before task text that contains a flag-shaped token"),
    (".codex-plugin/commands/collab.md", "frontmatter `argument-hint`",
     "start [--implementer=claude|codex] [--] <task>"),
    (".codex-plugin/commands/collab.md", "frontmatter `argument-hint`",
     "(`--` ends the flags: everything after it is literal text)"),
]
UNBOUNDED_FLAG_WORDING = "anywhere in the remaining token stream"


def mutate_flex_occurrence(path, snippet, index, replacement=MARK):
    """Replace only the `index`-th (0-based) occurrence of a flex phrase.

    The whole-file `mutate*` helpers prove a phrase is pinned SOMEWHERE in the
    file. That is the wrong granularity for a contract stated once per
    subcommand: deleting all three copies reds a file-wide search just as well
    as a per-section one, so only a single-copy mutation can tell them apart —
    and "the flags were added to `start` and `join` was left on the old parse"
    is precisely the single-copy case.
    """
    text = path.read_text()
    pattern = r"\s+".join(re.escape(part) for part in snippet.split())
    matches = list(re.finditer(pattern, text))
    assert len(matches) > index, (
        f"{path.name}: expected more than {index} occurrences of {snippet!r}, "
        f"found {len(matches)}")
    m = matches[index]
    path.write_text(text[:m.start()] + replacement + text[m.end():])


@pytest.mark.parametrize("surface,labels", TERMINATOR_SURFACE_LABELS)
@pytest.mark.parametrize("snippet", TERMINATOR_SHARED_SNIPPETS)
def test_lint_requires_the_terminator_contract_in_each_surface(
        tmp_path, surface, labels, snippet):
    # One surface at a time: this is the drift that shipped, with the `--`
    # rule live in two files and absent from the third.
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / surface, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    for label in labels:
        assert (f"{surface}: {label} is missing `--` end-of-options "
                f"terminator contract {snippet!r}") in r.stdout


@pytest.mark.parametrize("surface,index,expected,intact", [
    # `start`, `review`, `join` in document order in the Claude command file;
    # `start`, `join` in the Codex shim.
    (".claude-plugin/commands/collab.md", 2, "`join`", ["`start`", "`review`"]),
    (".claude-plugin/commands/collab.md", 0, "`start`", ["`review`", "`join`"]),
    (".codex-plugin/commands/collab.md", 1, "`join`", ["`start`"]),
])
def test_lint_requires_the_terminator_contract_in_each_subcommand(
        tmp_path, surface, index, expected, intact):
    # Within a surface the contract is stated once per subcommand, and the
    # regression shape is one subcommand left behind. Removing a single copy
    # must red, and must red for THAT subcommand only — a file-wide search
    # would stay green here because the other copies are untouched.
    snippet = "Flags are recognized only before the first `--`"
    fixture = copy_fixture(tmp_path)
    mutate_flex_occurrence(fixture / surface, snippet, index)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{surface}: {expected} is missing `--` end-of-options "
            f"terminator contract {snippet!r}") in r.stdout
    for label in intact:
        assert (f"{surface}: {label} is missing `--` end-of-options "
                f"terminator contract {snippet!r}") not in r.stdout


@pytest.mark.parametrize("surface,label,snippet", TERMINATOR_REGION_SNIPPETS)
def test_lint_requires_each_per_subcommand_terminator_snippet(
        tmp_path, surface, label, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / surface, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{surface}: {label} is missing `--` end-of-options terminator "
            f"contract {snippet!r}") in r.stdout


@pytest.mark.parametrize("surface,label,snippet", ESCAPE_HATCH_REGIONS)
def test_lint_requires_the_escape_hatch_in_every_flag_parsing_region(
        tmp_path, surface, label, snippet):
    # One region at a time. The terminator is only usable if the region a user
    # reads TELLS them to type `--`, and the shape that shipped is a region
    # that defines the terminator perfectly and never mentions the hatch — the
    # Codex shim's `start` had the worked example with no sentence, and its
    # `join` had neither. Deleting the sentence from one region must red for
    # THAT region, and must leave the other five unreported: a lint that
    # searched file-wide, or that pinned the hatch in only four of the six
    # regions, passes this file's other tests untouched.
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / surface, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{surface}: {label} is missing `--` end-of-options terminator "
            f"contract {snippet!r}") in r.stdout
    for other_surface, other_label, other_snippet in ESCAPE_HATCH_REGIONS:
        if (other_surface, other_label) == (surface, label):
            continue
        assert (f"{other_surface}: {other_label} is missing `--` "
                f"end-of-options terminator contract "
                f"{other_snippet!r}") not in r.stdout


def test_lint_requires_the_docs_flag_parsing_section_to_exist(tmp_path):
    # docs/COLLAB.md carried no flag-parsing contract at all until this round,
    # which is how the two command files came to disagree with nothing to
    # arbitrate between them. Renaming the heading must not silently drop the
    # spec's copy of the contract.
    fixture = copy_fixture(tmp_path)
    mutate(fixture / "docs" / "COLLAB.md", DOC_FLAG_PARSING_HEADING,
           "### Something else")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"docs/COLLAB.md: missing {DOC_FLAG_PARSING_HEADING!r}" in r.stdout


def test_lint_scopes_the_docs_flag_parsing_pins_to_their_own_subsection(
        tmp_path):
    # The section ends at the next `###`, not at the next `##`. Without that
    # boundary the section runs on through the rest of § Prompt Templates and
    # any sentence below it satisfies a pin scoped to the contract — so the
    # rule could be deleted from the section that states it and restored
    # anywhere downstream with the lint green.
    fixture = copy_fixture(tmp_path)
    doc = fixture / "docs" / "COLLAB.md"
    snippet = "The `--` itself is consumed — it is not part of the captured positional."
    mutate_flex(doc, snippet)
    mutate(doc, "### Starting a session (Claude's terminal — normal path)",
           f"### Decoy\n\n{snippet}\n\n"
           "### Starting a session (Claude's terminal — normal path)")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("docs/COLLAB.md: § `/collab` flag parsing is missing `--` "
            f"end-of-options terminator contract {snippet!r}") in r.stdout


@pytest.mark.parametrize("surface,label,snippet", TERMINATOR_USAGE_SNIPPETS)
def test_lint_requires_the_terminator_in_every_usage_surface(
        tmp_path, surface, label, snippet):
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / surface, snippet)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{surface}: {label} is missing `--` terminator usage "
            f"{snippet!r}") in r.stdout


@pytest.mark.parametrize("surface,index,expected,intact", [
    # `[--] <task>` in document order in the Claude command file: the
    # frontmatter `description`, the frontmatter `argument-hint`, then the
    # Unknown-subcommand usage block.
    (".claude-plugin/commands/collab.md", 0, "frontmatter `description`",
     ["frontmatter `argument-hint`", "§ `Unknown subcommand`"]),
    (".claude-plugin/commands/collab.md", 1, "frontmatter `argument-hint`",
     ["frontmatter `description`", "§ `Unknown subcommand`"]),
    (".claude-plugin/commands/collab.md", 2, "§ `Unknown subcommand`",
     ["frontmatter `description`", "frontmatter `argument-hint`"]),
    (".codex-plugin/commands/collab.md", 0, "frontmatter `argument-hint`", []),
])
def test_lint_requires_the_terminator_in_each_usage_region_separately(
        tmp_path, surface, index, expected, intact):
    # The same usage string appears three times in the Claude command file, so
    # a file-wide search cannot tell "all three advertise `[--]`" from "the
    # description advertises it three times". Dropping `[--]` from one of them
    # is the realistic edit, and it must red for that region alone.
    fixture = copy_fixture(tmp_path)
    mutate_flex_occurrence(fixture / surface, "[--] <task>", index, "<task>")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert f"{surface}: {expected} is missing `--` terminator usage" in r.stdout
    for label in intact:
        assert (f"{surface}: {label} is missing `--` terminator usage "
                "'/collab start") not in r.stdout
        assert (f"{surface}: {label} is missing `--` terminator usage "
                "'start [--") not in r.stdout


@pytest.mark.parametrize("surface", [".claude-plugin/commands/collab.md",
                                     ".codex-plugin/commands/collab.md",
                                     "docs/COLLAB.md"])
def test_lint_rejects_the_unterminated_flag_scan_wording(tmp_path, surface):
    # The pre-terminator wording, reintroduced with every positive pin left
    # intact — an ADDED sentence, not an edited one, so nothing else in the
    # lint can catch it. That is how it survived in the Codex shim after the
    # other two surfaces had been qualified: it reads as a simplification.
    fixture = copy_fixture(tmp_path)
    path = fixture / surface
    path.write_text(path.read_text() +
                    f"\n\nDetect the flag {UNBOUNDED_FLAG_WORDING}.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"{surface}:" in r.stdout
            and "flag detection is described over an unbounded stream"
            in r.stdout)
    assert "end-of-options terminator contract" not in r.stdout


@pytest.mark.parametrize("surface", [".claude-plugin/commands/collab.md",
                                     ".codex-plugin/commands/collab.md",
                                     "docs/COLLAB.md"])
def test_lint_rejects_the_qualifier_being_dropped_from_the_flag_scan(
        tmp_path, surface):
    # The same regression as an EDIT: the qualifier deleted from the live
    # sentence, which is the shape the shim actually shipped.
    fixture = copy_fixture(tmp_path)
    mutate_flex(fixture / surface, "anywhere in the token stream before the "
                "first `--`", UNBOUNDED_FLAG_WORDING)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "flag detection is described over an unbounded stream" in r.stdout


def test_lint_reports_an_ambiguous_codex_shim_flag_region_anchor(tmp_path):
    # The shim has no `##` headings, so its two flag-parsing regions are cut
    # from anchor sentences. A duplicated anchor silently re-scopes the region
    # every pin is checked against, so it is reported rather than resolved.
    fixture = copy_fixture(tmp_path)
    shim = fixture / ".codex-plugin" / "commands" / "collab.md"
    anchor = "For `join`, parse exactly one session id"
    shim.write_text(shim.read_text() + f"\n\n{anchor} plus flags.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".codex-plugin/commands/collab.md: the `start` flag-parsing "
            f"region is delimited by {anchor!r}, which appears 2 times") in r.stdout


# ------------------------------------------------------------------ task 11
#
# The mutating/read-only split for ultrareview's review lenses must be a
# machine-readable classification with one source of truth — ROSTER in
# ultrareview.js — never a sentence restated in prompt/command markdown, since
# a restatement is a second copy of the fact and a second copy drifts.
#
# Each `test_lint_rejects_*` case below is a literal reproduction of an
# example a code-quality review of the first cut of this check said was
# missed or wrongly flagged — not a paraphrase — so a regression in either
# direction reproduces the exact review finding, not a nearby approximation.

def test_lint_rejects_prose_that_contradicts_the_roster_mutation_classification(tmp_path):
    fixture = copy_fixture(tmp_path)
    path = fixture / ".claude-plugin" / "commands" / "ultrareview-local.md"
    # K is classified `mutates: true` in the real ROSTER; this sentence claims
    # the opposite in prose.
    path.write_text(path.read_text() +
                     "\n\nperformance-reviewer (K) is read-only and never "
                     "writes to the tree.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (".claude-plugin/commands/ultrareview-local.md:" in r.stdout
            and "restates the mutating/read-only classification for lens "
                "'K' in prose" in r.stdout)


def test_lint_rejects_a_generic_competing_lens_list_even_when_correct(tmp_path):
    fixture = copy_fixture(tmp_path)
    path = fixture / ".claude-plugin" / "commands" / "ultrareview-local.md"
    # Both ids are correct per the real ROSTER — restating them is still a
    # second source of truth, so it must fail too, not just a wrong one. This
    # phrasing ("lenses that mutate") names no single lens as its direct
    # subject, so it is caught by the generic competing-list detector rather
    # than the per-lens one.
    path.write_text(path.read_text() +
                     "\n\npr-test-analyzer (G) and performance-reviewer (K) "
                     "are the only lenses that mutate the working tree.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("describes a competing mutating/read-only lens list in prose"
            in r.stdout)


def test_lint_rejects_a_wrapped_per_lens_classification_sentence(tmp_path):
    # This corpus hard-wraps prose at ~78 columns, so a classification
    # sentence routinely spans two source lines. A line-at-a-time scan missed
    # this; the paragraph-joining fix must not.
    fixture = copy_fixture(tmp_path)
    path = fixture / ".claude-plugin" / "commands" / "ultrareview-local.md"
    path.write_text(path.read_text() +
                     "\n\nThe performance-reviewer lens\nis read-only and "
                     "never writes.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "restates the mutating/read-only classification for lens 'K' in prose" in r.stdout


@pytest.mark.parametrize("sentence,lens_id", [
    ("Agent K is read-only and never edits files.", "K"),
    ("Lens K is read-only.", "K"),
    ("**K** is read-only.", "K"),
    ("pr-test-analyzer (G) never writes to the working tree.", "G"),
    ("Lens G runs the test suite so it writes to the tree; all others do not.", "G"),
])
def test_lint_rejects_various_per_lens_classification_phrasings(tmp_path, sentence, lens_id):
    fixture = copy_fixture(tmp_path)
    path = fixture / ".claude-plugin" / "commands" / "ultrareview-local.md"
    path.write_text(path.read_text() + f"\n\n{sentence}\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert (f"restates the mutating/read-only classification for lens "
            f"{lens_id!r} in prose") in r.stdout


def test_lint_ignores_the_blanket_report_only_instruction(tmp_path):
    # `collab-turn-review-fix-global.md` already tells the pilot to "Treat the
    # review agents as read-only" under `--report-only` — a blanket statement
    # about the whole finding pass, naming no specific lens. It must stay
    # allowed; a green fixture (asserted by `test_fixture_is_green`) already
    # proves this, but the assertion is repeated here so a regression in the
    # detection regex reads as "this specific case broke", not a diff against
    # an unrelated failure elsewhere in the corpus.
    fixture = copy_fixture(tmp_path)
    r = run({"COLLAB_LINT_ROOT": str(fixture)})
    assert r.returncode == 0, r.stdout


@pytest.mark.parametrize("sentence", [
    # A lettered list, not a lens reference — case-insensitive id matching
    # once turned "(a)"/"(b)" into false positives for lens A/B.
    "The pass is read-only in three senses: (a) no edits, (b) no commits.",
    # Names a lens near a mutation word without classifying THAT lens — the
    # find phase is read-only, not the architect lens.
    "The architect lens is dispatched during the read-only find phase.",
    # The sanctioned way to describe the policy without a second copy of it —
    # must be reachable, or the fix can't be documented in the files this
    # check governs.
    "Whether a lens like performance-reviewer mutates is declared by "
    "ROSTER.mutates; do not restate it here.",
    # Task 12's own forward-looking wording (issue #265 hardening plan, task
    # 12 step 4): a generic policy description that points back to
    # ROSTER.mutates must stay allowed, since task 12 needs to write exactly
    # this.
    "mutating lenses run in an isolated worktree; see ROSTER.mutates for "
    "which ones",
])
def test_lint_does_not_flag_prose_that_only_looks_like_a_classification(tmp_path, sentence):
    fixture = copy_fixture(tmp_path)
    path = fixture / ".claude-plugin" / "commands" / "ultrareview-local.md"
    path.write_text(path.read_text() + f"\n\n{sentence}\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 0, r.stdout


def test_lint_covers_codex_prompts_not_naming_ultrareview_by_substring(tmp_path):
    # `.codex-plugin/prompts/collab-review-local.md` is the Codex-side
    # counterpart of what a later task edits, and it never contains the
    # literal string "ultrareview" — the check used to gate its file scope on
    # that substring, which skipped this file (and its two pre-existing
    # read-only sentences) entirely. The scope must not be gated on it.
    fixture = copy_fixture(tmp_path)
    path = fixture / ".codex-plugin" / "prompts" / "collab-review-local.md"
    assert "ultrareview" not in path.read_text().lower()
    path.write_text(path.read_text() +
                     "\n\nAgent K is read-only and never edits files.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ".codex-plugin/prompts/collab-review-local.md:" in r.stdout
    assert "restates the mutating/read-only classification for lens 'K' in prose" in r.stdout


def test_lint_derives_lens_names_from_the_roster_instead_of_a_second_list(tmp_path):
    # If the check carried its own hardcoded name-to-id table (the shape of
    # the original cut of this check), renaming a lens in ROSTER would leave
    # prose using the OLD name uncovered and prose using the NEW name
    # unrecognised — a second copy of roster metadata going stale exactly the
    # way this task exists to prevent. Renaming K here and referencing it by
    # the new name only must still be caught.
    fixture = copy_fixture(tmp_path)
    js_path = fixture / ".claude-plugin" / "workflows" / "ultrareview.js"
    mutate(js_path, "key: 'performance-reviewer'", "key: 'query-cost-reviewer'")
    md_path = fixture / ".claude-plugin" / "commands" / "ultrareview-local.md"
    md_path.write_text(md_path.read_text() +
                        "\n\nquery-cost-reviewer is read-only and never "
                        "writes to the tree.\n")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "restates the mutating/read-only classification for lens 'K' in prose" in r.stdout


def test_lint_rejects_a_roster_entry_missing_the_mutates_field(tmp_path):
    fixture = copy_fixture(tmp_path)
    path = fixture / ".claude-plugin" / "workflows" / "ultrareview.js"
    mutate(path,
           "agentType: toolkit('pr-test-analyzer'), model: 'sonnet', "
           "effort: 'high', fable: false, mutates: true",
           "agentType: toolkit('pr-test-analyzer'), model: 'sonnet', "
           "effort: 'high', fable: false")

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("ultrareview.js: ROSTER entries ['G'] do not declare "
            "mutates: true|false") in r.stdout
