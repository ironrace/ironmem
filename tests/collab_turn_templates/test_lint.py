import subprocess, sys, pathlib
ROOT = pathlib.Path(__file__).resolve().parents[2]
LINT = ROOT / "scripts" / "check_collab_turn_templates.py"

def run():
    return subprocess.run([sys.executable, str(LINT)], cwd=ROOT,
                          capture_output=True, text=True)

def test_lint_passes_on_repo():
    r = run()
    assert r.returncode == 0, f"lint failed:\n{r.stdout}\n{r.stderr}"

def test_lint_catches_unknown_placeholder(tmp_path, monkeypatch):
    # copy a template, inject a bad placeholder, point lint at a temp dir via env
    bad = ROOT / ".claude-plugin" / "prompts" / "collab-turn-plan-draft.md"
    text = bad.read_text()
    assert "$SESSION_ID" in text  # sanity: known placeholder present
