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
    return fixture


def test_lint_passes_on_repo():
    r = run()
    assert r.returncode == 0, f"lint failed:\n{r.stdout}\n{r.stderr}"


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
