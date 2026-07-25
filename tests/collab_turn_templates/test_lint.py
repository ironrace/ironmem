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


def test_lint_passes_on_repo():
    r = run()
    assert r.returncode == 0, f"lint failed:\n{r.stdout}\n{r.stderr}"


def test_codex_dispatch_uses_explicit_repository_model_defaults():
    docs = (ROOT / "docs" / "COLLAB.md").read_text()
    dispatcher = (ROOT / ".claude-plugin" / "commands" / "collab.md").read_text()
    plan_draft_prompt = (ROOT / ".codex-plugin" / "prompts" / "collab-plan-draft.md").read_text()
    plan_review_prompt = (ROOT / ".codex-plugin" / "prompts" / "collab-plan-review.md").read_text()
    global_review_prompt = (ROOT / ".codex-plugin" / "prompts" / "collab-global-review.md").read_text()
    recovery_prompt = (ROOT / ".codex-plugin" / "prompts" / "collab-recovery.md").read_text()
    batch_prompt = (ROOT / ".codex-plugin" / "prompts" / "collab-batch-impl.md").read_text()
    tools = (ROOT / ".codex-plugin" / "skills" / "using-superpowers" /
             "references" / "codex-tools.md").read_text()
    source_tools = (ROOT / ".claude-plugin" / "skills" / "using-superpowers" /
                    "references" / "codex-tools.md").read_text()
    agents = (ROOT / "AGENTS.md").read_text()

    for surface in (docs, dispatcher, plan_draft_prompt, plan_review_prompt,
                    global_review_prompt, recovery_prompt, batch_prompt, tools,
                    source_tools, agents):
        assert "gpt-5.6-luna" in surface
        assert "gpt-5.6-terra" in surface
        assert "gpt-5.6-sol" in surface

    assert "CodeImplementPending" in dispatcher
    assert "-m gpt-5.6-luna -c model_reasoning_effort=max" in dispatcher
    assert "-m gpt-5.6-terra -c model_reasoning_effort=high" in dispatcher
    assert "codex exec --prompt-file" not in dispatcher
    assert "model_reasoning_effort=xhigh" not in dispatcher
    assert "personal Codex default" in docs
    assert "escalation tier, not the default" in plan_draft_prompt


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
