#!/usr/bin/env python3
"""Self-test for scripts/run_git_hook.py classification logic."""
from __future__ import annotations

import importlib.util
import pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
HOOK = ROOT / "scripts" / "run_git_hook.py"


def load_hook_module():
    spec = importlib.util.spec_from_file_location("run_git_hook", HOOK)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main() -> int:
    hook = load_hook_module()

    collab, rust, hooks = hook.gate_summary(["docs/COLLAB.md"])
    assert collab and not rust and not hooks

    collab, rust, hooks = hook.gate_summary([".claude-plugin/prompts/collab-turn-code-implement.md"])
    assert collab and not rust and not hooks

    collab, rust, hooks = hook.gate_summary(["crates/ironmem/src/hook.rs"])
    assert rust and not collab and not hooks

    collab, rust, hooks = hook.gate_summary(["crates/ironmem/Cargo.toml"])
    assert rust and not collab and not hooks

    collab, rust, hooks = hook.gate_summary(["README.md"])
    assert not collab and not rust and not hooks

    collab, rust, hooks = hook.gate_summary(["scripts/run_git_hook.py"])
    assert hooks and not collab and not rust

    print("run_git_hook self-test passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
