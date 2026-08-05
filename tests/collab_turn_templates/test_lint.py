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
    "**Every other phase:** as in condition 5 — the planning phases are",
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
    "more than 10 tasks",
    "PlanLocked is pre-coding",
    "plan_file_path",
    'collab_send(sender="$SENDER", topic="task_list",',
    'Verify `$SENDER` against `collab_status.current_owner`',
    'MUST NOT be\n   substituted with your own identity',
    'equal `current_owner`, ABORT — do not send anything — and report the',
    'always the pilot, which under `pilot == "codex"` is `codex`',
]


def test_fixture_is_green(tmp_path):
    # The premise every `assert r.returncode == 1` below depends on.
    fixture = copy_fixture(tmp_path)

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 0, f"unmutated fixture must lint clean:\n{r.stdout}\n{r.stderr}"


def test_lint_passes_on_repo():
    r = run()
    assert r.returncode == 0, f"lint failed:\n{r.stdout}\n{r.stderr}"


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
