#!/usr/bin/env python3
"""Tests for scripts/run_git_hook.py.

Invoked directly by the tracked Git hooks as
``python3 scripts/test_run_git_hook.py`` (see .githooks/pre-commit and
.githooks/pre-push), and also runnable normally via ``pytest`` or
``pytest scripts/test_run_git_hook.py``.
"""
from __future__ import annotations

import dataclasses
import importlib.util
import pathlib
import sys
from types import MappingProxyType

try:
    import pytest
except ImportError:  # pragma: no cover - exercised only when pytest is absent
    pytest = None  # type: ignore[assignment]

ROOT = pathlib.Path(__file__).resolve().parents[1]
HOOK = ROOT / "scripts" / "run_git_hook.py"


def load_hook_module():
    spec = importlib.util.spec_from_file_location("run_git_hook", HOOK)
    assert spec and spec.loader
    module = importlib.util.module_from_spec(spec)
    # dataclasses (frozen=True, `from __future__ import annotations`) resolves
    # type hints via sys.modules[cls.__module__]; the module must be
    # registered there before exec_module runs the class bodies.
    sys.modules[spec.name] = module
    try:
        spec.loader.exec_module(module)
    except BaseException:
        # Don't leave a partially-initialized module registered under a
        # shared name if exec_module fails partway through.
        sys.modules.pop(spec.name, None)
        raise
    return module


hook = load_hook_module()


# --- path classification (ported from the former plain-assert self-test) ---


def test_gate_summary_collab_exact_path():
    collab, rust, hooks = hook.gate_summary(["docs/COLLAB.md"])
    assert collab and not rust and not hooks


def test_gate_summary_collab_turn_prefix():
    collab, rust, hooks = hook.gate_summary(
        [".claude-plugin/prompts/collab-turn-code-implement.md"]
    )
    assert collab and not rust and not hooks


def test_gate_summary_rust_source():
    collab, rust, hooks = hook.gate_summary(["crates/ironmem/src/hook.rs"])
    assert rust and not collab and not hooks


def test_gate_summary_rust_cargo_toml():
    collab, rust, hooks = hook.gate_summary(["crates/ironmem/Cargo.toml"])
    assert rust and not collab and not hooks


def test_gate_summary_docs_only():
    collab, rust, hooks = hook.gate_summary(["README.md"])
    assert not collab and not rust and not hooks


def test_gate_summary_hook_path():
    collab, rust, hooks = hook.gate_summary(["scripts/run_git_hook.py"])
    assert hooks and not collab and not rust


# --- Task 1: Gate is frozen ---


def test_gate_is_frozen():
    gate = hook.Gate(
        name="example",
        argv=("python3", "scripts/example.py"),
        phases=frozenset({"pre-commit"}),
        surfaces=frozenset({hook.SURFACE_HOOK_SELF_TEST}),
        always=False,
    )
    with pytest.raises(dataclasses.FrozenInstanceError):
        gate.name = "mutated"  # type: ignore[misc]


def test_gate_rejects_non_str_name():
    with pytest.raises(TypeError):
        hook.Gate(
            name=123,  # type: ignore[arg-type]
            argv=("python3", "scripts/example.py"),
            phases=frozenset({"pre-commit"}),
            surfaces=frozenset({hook.SURFACE_HOOK_SELF_TEST}),
            always=False,
        )


def test_gate_rejects_non_tuple_argv():
    with pytest.raises(TypeError):
        hook.Gate(
            name="example",
            argv=["python3", "scripts/example.py"],  # type: ignore[arg-type]
            phases=frozenset({"pre-commit"}),
            surfaces=frozenset({hook.SURFACE_HOOK_SELF_TEST}),
            always=False,
        )


def test_gate_rejects_non_frozenset_phases():
    with pytest.raises(TypeError):
        hook.Gate(
            name="example",
            argv=("python3", "scripts/example.py"),
            phases={"pre-commit"},  # type: ignore[arg-type]
            surfaces=frozenset({hook.SURFACE_HOOK_SELF_TEST}),
            always=False,
        )


def test_gate_rejects_non_frozenset_surfaces():
    with pytest.raises(TypeError):
        hook.Gate(
            name="example",
            argv=("python3", "scripts/example.py"),
            phases=frozenset({"pre-commit"}),
            surfaces={hook.SURFACE_HOOK_SELF_TEST},  # type: ignore[arg-type]
            always=False,
        )


def test_gate_rejects_non_bool_always():
    with pytest.raises(TypeError):
        hook.Gate(
            name="example",
            argv=("python3", "scripts/example.py"),
            phases=frozenset({"pre-commit"}),
            surfaces=frozenset({hook.SURFACE_HOOK_SELF_TEST}),
            always="false",  # type: ignore[arg-type]
        )


# --- Task 1: ChangeSet is frozen ---


def test_changeset_is_frozen():
    changeset = hook.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(dataclasses.FrozenInstanceError):
        changeset.unknown = True  # type: ignore[misc]


def test_changeset_rejects_non_tuple_paths():
    with pytest.raises(TypeError):
        hook.ChangeSet(paths=["a.py"], unknown=False, reason=None)  # type: ignore[arg-type]


def test_changeset_rejects_non_bool_unknown():
    with pytest.raises(TypeError):
        hook.ChangeSet(paths=(), unknown="false", reason=None)  # type: ignore[arg-type]


def test_changeset_rejects_non_str_reason():
    with pytest.raises(TypeError):
        hook.ChangeSet(paths=(), unknown=True, reason=404)  # type: ignore[arg-type]


def test_changeset_accepts_none_reason():
    changeset = hook.ChangeSet(paths=(), unknown=False, reason=None)
    assert changeset.reason is None


def test_changeset_default_construction():
    changeset = hook.ChangeSet(paths=(), unknown=False, reason=None)
    assert changeset.paths == ()
    assert changeset.unknown is False
    assert changeset.reason is None


def test_changeset_default_is_distinct_from_escalated():
    default = hook.ChangeSet(paths=(), unknown=False, reason=None)
    escalated = hook.ChangeSet(
        paths=("weird\x00path",), unknown=True, reason="null byte in path"
    )
    assert default != escalated
    assert escalated.unknown is True
    assert escalated.reason == "null byte in path"


# --- Task 1: manifest is a tuple, declaration order is execution order ---


def test_manifest_is_a_tuple():
    assert isinstance(hook.GATES, tuple)
    with pytest.raises(AttributeError):
        hook.GATES.append(hook.GATES[0])  # type: ignore[attr-defined]


def test_manifest_declaration_order_is_preserved():
    # This is the literal authored order. If anything ever sorts GATES at
    # runtime this test must fail, because the authored order below is not
    # alphabetical (see test_manifest_declaration_order_is_not_alphabetical).
    names = [gate.name for gate in hook.GATES]
    assert names == [
        "hook_self_test",
        "hook_install_check",
        "collab_template_lint",
        "rust_fmt_check",
        "rust_clippy",
        "rust_test",
    ]


def test_manifest_declaration_order_is_not_alphabetical():
    # Proves the preceding order-equality test is a meaningful check for "no
    # runtime sort", not an accident of already-sorted input.
    names = [gate.name for gate in hook.GATES]
    assert names != sorted(names)


# --- Task 1: manifest gates match today's run_pre_commit/run_pre_push ---


def test_manifest_matches_pre_commit_argv_and_order():
    expected = [
        ("python3", "scripts/test_run_git_hook.py"),
        ("bash", "scripts/install-git-hooks.sh", "--check"),
        ("python3", "scripts/check_collab_turn_templates.py"),
        ("cargo", "fmt", "--all", "--", "--check"),
        (
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ),
    ]
    actual = [gate.argv for gate in hook.GATES if "pre-commit" in gate.phases]
    assert actual == expected


def test_manifest_matches_pre_push_argv_and_order():
    expected = [
        ("python3", "scripts/test_run_git_hook.py"),
        ("python3", "scripts/check_collab_turn_templates.py"),
        ("cargo", "test", "--workspace"),
    ]
    actual = [gate.argv for gate in hook.GATES if "pre-push" in gate.phases]
    assert actual == expected


def test_manifest_gate_argv_entries_are_string_literals():
    for gate in hook.GATES:
        for entry in gate.argv:
            assert isinstance(entry, str)


def test_manifest_no_gate_marked_always_yet():
    # None of today's ported gates run unconditionally; `always` exists for
    # future gates but nothing in the current set uses it.
    assert all(gate.always is False for gate in hook.GATES)


# --- Task 1: surface map ---


def test_surfaces_contains_expected_ids():
    assert set(hook.SURFACES) == {
        hook.SURFACE_RUST_WORKSPACE,
        hook.SURFACE_COLLAB_PROTOCOL,
        hook.SURFACE_HOOK_SELF_TEST,
    }


def test_surfaces_ported_unchanged_from_existing_predicates():
    assert hook.SURFACES[hook.SURFACE_RUST_WORKSPACE] is hook.is_rust_path
    assert hook.SURFACES[hook.SURFACE_COLLAB_PROTOCOL] is hook.is_collab_protocol_path
    assert hook.SURFACES[hook.SURFACE_HOOK_SELF_TEST] is hook.is_hook_path


def test_surfaces_is_a_frozen_mapping():
    assert isinstance(hook.SURFACES, MappingProxyType)
    with pytest.raises(TypeError):
        hook.SURFACES["new_surface"] = lambda path: False  # type: ignore[index]


def test_every_gate_surface_id_is_registered():
    for gate in hook.GATES:
        for surface_id in gate.surfaces:
            assert surface_id in hook.SURFACES


# --- __main__ delegation must fail loudly, never exit 0, if pytest is absent ---


def test_run_as_script_fails_loudly_without_pytest(monkeypatch, capsys):
    this_module = sys.modules[__name__]
    monkeypatch.setattr(this_module, "pytest", None)
    exit_code = _run_as_script()
    assert exit_code != 0
    captured = capsys.readouterr()
    assert "pytest" in captured.err.lower()


def _run_as_script() -> int:
    if pytest is None:
        sys.stderr.write(
            "ERROR: pytest is required to run scripts/test_run_git_hook.py but is "
            "not installed.\nInstall it with: pip install pytest\n"
        )
        return 1
    return pytest.main([__file__])


if __name__ == "__main__":
    raise SystemExit(_run_as_script())
