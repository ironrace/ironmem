import os, pathlib, shutil, subprocess, sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
LINT = ROOT / "scripts" / "check_collab_turn_templates.py"


def run(extra_env=None):
    env = os.environ.copy()
    if extra_env:
        env.update(extra_env)
    return subprocess.run([sys.executable, str(LINT)], cwd=ROOT, env=env,
                          capture_output=True, text=True)


def copy_fixture(tmp_path):
    fixture = tmp_path / "repo"
    shutil.copytree(ROOT / ".claude-plugin" / "prompts",
                    fixture / ".claude-plugin" / "prompts")
    shutil.copytree(ROOT / ".claude-plugin" / "commands",
                    fixture / ".claude-plugin" / "commands")
    shutil.copytree(ROOT / ".codex-plugin" / "prompts",
                    fixture / ".codex-plugin" / "prompts")
    (fixture / "docs").mkdir()
    shutil.copy2(ROOT / "docs" / "COLLAB.md", fixture / "docs" / "COLLAB.md")
    shutil.copy2(ROOT / "docs" / "EVALUATE_ISSUE.md", fixture / "docs" / "EVALUATE_ISSUE.md")
    return fixture


def copy_phase_rs(fixture):
    """The phase-name check derives its valid set from phase.rs."""
    rel = pathlib.Path("crates") / "ironmem" / "src" / "collab" / "phase.rs"
    (fixture / rel).parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(ROOT / rel, fixture / rel)


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
    scripts = fixture / "scripts"
    scripts.mkdir()
    installer = ROOT / "scripts" / "install-ironmem.sh"
    (scripts / "install-ironmem.sh").write_text(
        installer.read_text().replace("  collab-turn-plan-review\n", "", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert "REQUIRED_CLAUDE_PROMPTS is missing 'collab-turn-plan-review'" in r.stdout


def test_lint_rejects_a_precondition_naming_a_rust_variant(tmp_path):
    # `collab_status` emits `wire_name`, so a `preconditions:` line naming the
    # Rust variant of a genericized phase fails closed on every dispatch. The
    # valid set is parsed out of phase.rs, so the fixture needs that file.
    fixture = copy_fixture(tmp_path)
    copy_phase_rs(fixture)
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
    copy_phase_rs(fixture)
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
    copy_phase_rs(fixture)
    template = fixture / ".claude-plugin" / "prompts" / "collab-turn-task-list.md"
    template.write_text(template.read_text().replace(
        "preconditions: phase == PlanLocked,", "preconditions: phase is PlanLocked,", 1))

    r = run({"COLLAB_LINT_ROOT": str(fixture)})

    assert r.returncode == 1
    assert ("collab-turn-task-list.md: preconditions mentions a phase but "
            "names none in the `phase == <WireName>` form") in r.stdout


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
