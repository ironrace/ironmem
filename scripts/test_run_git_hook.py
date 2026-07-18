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
        hook.SURFACE_DOCS,
    }


def test_surfaces_ported_unchanged_from_existing_predicates():
    assert hook.SURFACES[hook.SURFACE_RUST_WORKSPACE] is hook.is_rust_path
    assert hook.SURFACES[hook.SURFACE_COLLAB_PROTOCOL] is hook.is_collab_protocol_path
    assert hook.SURFACES[hook.SURFACE_HOOK_SELF_TEST] is hook.is_hook_path


def test_surfaces_docs_entry_is_is_docs_path():
    assert hook.SURFACES[hook.SURFACE_DOCS] is hook.is_docs_path


def test_surfaces_is_a_frozen_mapping():
    assert isinstance(hook.SURFACES, MappingProxyType)
    with pytest.raises(TypeError):
        hook.SURFACES["new_surface"] = lambda path: False  # type: ignore[index]


def test_every_gate_surface_id_is_registered():
    for gate in hook.GATES:
        for surface_id in gate.surfaces:
            assert surface_id in hook.SURFACES


# --- Task 2: is_docs_path -----------------------------------------------


def test_is_docs_path_matches_markdown_suffix():
    assert hook.is_docs_path("README.md") is True
    assert hook.is_docs_path("AGENTS.md") is True


def test_is_docs_path_matches_top_level_docs_directory():
    assert hook.is_docs_path("docs/CODEX.md") is True
    assert hook.is_docs_path("docs/superpowers/plans/notes.txt") is True


def test_is_docs_path_rejects_look_alike_directory():
    # "docsite/" is not "docs/" -- a substring check ("docs" in path) would
    # wrongly match this; a segment/prefix check must not.
    assert hook.is_docs_path("docsite/architecture.txt") is False


def test_is_docs_path_rejects_non_markdown_non_docs_path():
    assert hook.is_docs_path("crates/ironmem/src/hook.rs") is False


# --- Task 2: UNKNOWN is a distinct fallback, not a declared surface -----


def test_unknown_is_not_a_declared_surface():
    assert hook.UNKNOWN not in hook.SURFACES


# --- Task 2: classify_path -- known surfaces ----------------------------


def test_classify_path_rust_source():
    assert hook.classify_path("crates/ironmem/src/hook.rs") == hook.SURFACE_RUST_WORKSPACE


def test_classify_path_collab_exact_path():
    assert hook.classify_path("scripts/check_collab_turn_templates.py") == (
        hook.SURFACE_COLLAB_PROTOCOL
    )


def test_classify_path_hook_self_test_run_git_hook():
    assert hook.classify_path("scripts/run_git_hook.py") == hook.SURFACE_HOOK_SELF_TEST


def test_classify_path_hook_self_test_test_run_git_hook():
    assert hook.classify_path("scripts/test_run_git_hook.py") == hook.SURFACE_HOOK_SELF_TEST


def test_classify_path_docs_markdown_file():
    assert hook.classify_path("README.md") == hook.SURFACE_DOCS


def test_classify_path_docs_directory():
    assert hook.classify_path("docs/superpowers/plans/notes.txt") == hook.SURFACE_DOCS


def test_classify_path_known_surface_beats_generic_docs():
    # docs/COLLAB.md is both under docs/ and in the collab-protocol exact
    # set. The more specific declared surface wins over the generic inert
    # docs catch-all -- DOCS is checked last, never first.
    assert hook.classify_path("docs/COLLAB.md") == hook.SURFACE_COLLAB_PROTOCOL


# --- Task 2: classify_path -- near-misses classify UNKNOWN, not the surface
# they resemble -----------------------------------------------------------


def test_classify_path_near_miss_contests_is_not_collab_protocol():
    assert hook.classify_path("contests/collab_turn_templates/example.txt") == hook.UNKNOWN


def test_classify_path_near_miss_docsite_is_not_docs():
    assert hook.classify_path("docsite/architecture.txt") == hook.UNKNOWN


def test_classify_path_near_miss_src_backup_is_unknown():
    assert hook.classify_path("src_backup/lib.py") == hook.UNKNOWN


def test_classify_path_unrecognized_safe_shape_is_unknown():
    assert hook.classify_path("notes.txt") == hook.UNKNOWN


# --- Task 2: classify_path -- unsafe shapes classify UNKNOWN by rejection,
# never by crash and never by cleaning ------------------------------------


def test_classify_path_absolute_path_is_unknown():
    assert hook.classify_path("/etc/passwd") == hook.UNKNOWN


def test_classify_path_dotdot_segment_is_unknown():
    assert hook.classify_path("scripts/../etc/passwd") == hook.UNKNOWN


def test_classify_path_bare_dotdot_segment_is_unknown():
    assert hook.classify_path("..") == hook.UNKNOWN


def test_classify_path_nul_byte_is_unknown():
    assert hook.classify_path("weird\x00path.md") == hook.UNKNOWN


def test_classify_path_control_byte_is_unknown():
    assert hook.classify_path("weird\x1bpath.md") == hook.UNKNOWN


def test_classify_path_empty_string_is_unknown():
    assert hook.classify_path("") == hook.UNKNOWN


def test_classify_path_leading_dash_is_unknown():
    # Even though the extension would otherwise match the Rust surface, the
    # unsafe leading '-' shape check is rejected before surface matching.
    assert hook.classify_path("-danger.rs") == hook.UNKNOWN


def test_classify_path_non_str_int_is_unknown():
    assert hook.classify_path(123) == hook.UNKNOWN


def test_classify_path_non_str_none_is_unknown():
    assert hook.classify_path(None) == hook.UNKNOWN


def test_classify_path_non_str_list_is_unknown():
    assert hook.classify_path(["scripts/run_git_hook.py"]) == hook.UNKNOWN


def test_classify_path_never_raises_on_unsafe_shapes():
    # Escalation, not a crash: none of these may propagate an exception.
    unsafe_inputs = [
        "/etc/passwd",
        "..",
        "weird\x00path",
        "weird\x1bpath",
        "",
        "-rf",
        123,
        None,
        ["a"],
    ]
    for value in unsafe_inputs:
        assert hook.classify_path(value) == hook.UNKNOWN


# --- Task 2: paths are matched byte-exact -- no strip/case-fold/rewrite --


def test_classify_path_preserves_newline_space_and_non_ascii_segments():
    path = "docs/plan\n notes (β).md"
    assert hook.classify_path(path) == hook.SURFACE_DOCS
    # Segment-based matching operated on the real, unmodified bytes.
    assert path.split("/") == ["docs", "plan\n notes (β).md"]


def test_classify_path_does_not_strip_whitespace_before_matching():
    # If classify_path stripped the path before matching, this would
    # collapse to the exact hook-self-test path and misclassify. Byte-exact
    # matching must leave it unrecognized instead.
    path = " scripts/run_git_hook.py"
    assert hook.classify_path(path) == hook.UNKNOWN


def test_classify_path_does_not_strip_trailing_newline_before_matching():
    path = "scripts/run_git_hook.py\n"
    assert hook.classify_path(path) == hook.UNKNOWN


# --- Task 3: resolve_gates -- unknown phase raises -----------------------


def test_resolve_gates_unknown_phase_raises():
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(ValueError):
        hook.resolve_gates("typo-phase", changes)


def test_resolve_gates_empty_phase_string_raises():
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(ValueError):
        hook.resolve_gates("", changes)


# --- Task 3: resolve_gates -- phase filtering tested both directions -----


def test_resolve_gates_pre_commit_excludes_pre_push_only_gate():
    changes = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    names = [gate.name for gate in hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)]
    assert names == ["rust_fmt_check", "rust_clippy"]
    assert "rust_test" not in names


def test_resolve_gates_pre_push_excludes_pre_commit_only_gates():
    changes = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    names = [gate.name for gate in hook.resolve_gates(hook.PHASE_PRE_PUSH, changes)]
    assert names == ["rust_test"]
    assert "rust_fmt_check" not in names
    assert "rust_clippy" not in names


# --- Task 3: resolve_gates -- docs inert, unknown dominates ---------------


def test_resolve_gates_docs_only_selects_no_gates():
    # No gate in today's manifest is marked always=True (see
    # test_manifest_no_gate_marked_always_yet), so an all-docs change with a
    # known shape selects nothing: DOCS is inert, not an escalation trigger.
    changes = hook.ChangeSet(paths=("README.md",), unknown=False, reason=None)
    assert hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes) == ()
    assert hook.resolve_gates(hook.PHASE_PRE_PUSH, changes) == ()


def test_resolve_gates_docs_plus_code_path_does_not_skip():
    changes = hook.ChangeSet(
        paths=("README.md", "crates/ironmem/src/hook.rs"), unknown=False, reason=None
    )
    names = [gate.name for gate in hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)]
    assert names == ["rust_fmt_check", "rust_clippy"]


def test_resolve_gates_unrecognized_path_alone_runs_every_gate_for_phase():
    # "notes.txt" is a safe shape but classifies UNKNOWN (no declared surface
    # matches it). classify_path()'s UNKNOWN is the escalation signal, same
    # as changes.unknown=True -- it forces every phase-matching gate to run.
    changes = hook.ChangeSet(paths=("notes.txt",), unknown=False, reason=None)
    result = hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)
    expected = tuple(gate for gate in hook.GATES if hook.PHASE_PRE_COMMIT in gate.phases)
    assert result == expected


def test_resolve_gates_docs_plus_unrecognized_runs_every_gate_unknown_dominates():
    changes = hook.ChangeSet(
        paths=("README.md", "notes.txt"), unknown=False, reason=None
    )
    result = hook.resolve_gates(hook.PHASE_PRE_PUSH, changes)
    expected = tuple(gate for gate in hook.GATES if hook.PHASE_PRE_PUSH in gate.phases)
    assert result == expected


# --- Task 3: resolve_gates -- changes.unknown=True dominates paths --------


def test_resolve_gates_unknown_true_selects_full_phase_set_regardless_of_paths():
    changes = hook.ChangeSet(paths=("README.md",), unknown=True, reason="git diff failed")
    result = hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)
    expected = tuple(gate for gate in hook.GATES if hook.PHASE_PRE_COMMIT in gate.phases)
    assert result == expected


def test_resolve_gates_unknown_true_with_empty_paths_selects_full_phase_set():
    changes = hook.ChangeSet(paths=(), unknown=True, reason="malformed stdin")
    result = hook.resolve_gates(hook.PHASE_PRE_PUSH, changes)
    expected = tuple(gate for gate in hook.GATES if hook.PHASE_PRE_PUSH in gate.phases)
    assert result == expected


# --- Task 3: resolve_gates -- empty paths, unknown=False escalates nothing


def test_resolve_gates_empty_paths_unknown_false_selects_only_always_gates():
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    assert hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes) == ()
    assert hook.resolve_gates(hook.PHASE_PRE_PUSH, changes) == ()


# --- Task 3: resolve_gates -- output order is manifest order, invariant to
# input path order and duplicates; dedupe never changes the result ---------


def test_resolve_gates_output_order_is_manifest_order_invariant_to_input_order():
    forward = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs", "docs/COLLAB.md"),
        unknown=False,
        reason=None,
    )
    reordered = hook.ChangeSet(
        paths=("docs/COLLAB.md", "crates/ironmem/src/hook.rs"),
        unknown=False,
        reason=None,
    )
    result_forward = hook.resolve_gates(hook.PHASE_PRE_COMMIT, forward)
    result_reordered = hook.resolve_gates(hook.PHASE_PRE_COMMIT, reordered)
    assert result_forward == result_reordered
    # Manifest order (see test_manifest_declaration_order_is_preserved), not
    # input order: collab_template_lint is declared before the rust gates.
    assert [gate.name for gate in result_forward] == [
        "collab_template_lint",
        "rust_fmt_check",
        "rust_clippy",
    ]


def test_resolve_gates_output_invariant_to_duplicate_paths():
    deduped = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    duplicated = hook.ChangeSet(
        paths=(
            "crates/ironmem/src/hook.rs",
            "crates/ironmem/src/hook.rs",
            "crates/ironmem/src/hook.rs",
        ),
        unknown=False,
        reason=None,
    )
    assert hook.resolve_gates(hook.PHASE_PRE_COMMIT, deduped) == hook.resolve_gates(
        hook.PHASE_PRE_COMMIT, duplicated
    )


def test_resolve_gates_dedupes_by_first_seen_not_by_sorting():
    # Monkeypatch-free: assert on classify_path call order via a spy that
    # wraps the real function, proving resolve_gates visits each distinct
    # path exactly once, in first-seen order -- never a sorted order, which
    # would reorder "notes_b.txt" before "notes_a.txt".
    calls: list[str] = []
    original = hook.classify_path

    def spy(path):
        calls.append(path)
        return original(path)

    changes = hook.ChangeSet(
        paths=("notes_b.txt", "notes_a.txt", "notes_b.txt", "notes_a.txt"),
        unknown=False,
        reason=None,
    )
    real_classify_path = hook.classify_path
    hook.classify_path = spy
    try:
        hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)
    finally:
        hook.classify_path = real_classify_path

    assert calls == ["notes_b.txt", "notes_a.txt"]


# --- Task 3: resolve_gates -- overlapping surfaces select each gate once --


def test_resolve_gates_overlapping_surfaces_select_each_gate_exactly_once():
    # Two different paths that both classify to SURFACE_HOOK_SELF_TEST must
    # not duplicate hook_self_test / hook_install_check in the result.
    changes = hook.ChangeSet(
        paths=("scripts/run_git_hook.py", "scripts/install-git-hooks.sh"),
        unknown=False,
        reason=None,
    )
    result = hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)
    names = [gate.name for gate in result]
    assert names == ["hook_self_test", "hook_install_check"]
    assert len(names) == len(set(names))


# --- Task 3: resolve_gates -- returns a new tuple, never mutates inputs ---


def test_resolve_gates_returns_a_tuple():
    changes = hook.ChangeSet(paths=(), unknown=True, reason="test")
    result = hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)
    assert isinstance(result, tuple)


def test_resolve_gates_does_not_mutate_changeset_paths():
    original_paths = ("crates/ironmem/src/hook.rs", "crates/ironmem/src/hook.rs")
    changes = hook.ChangeSet(paths=original_paths, unknown=False, reason=None)
    hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)
    assert changes.paths == original_paths


# --- Task 3: resolve_gates -- parametrized per-gate reachability ----------
#
# Derived from GATES itself, not a hardcoded list of gate names: a future
# gate appended to the manifest without an entry in
# _SURFACE_EXAMPLE_PATH_FOR_TEST below fails with a KeyError right here,
# rather than silently going unexercised.

_SURFACE_EXAMPLE_PATH_FOR_TEST = {
    hook.SURFACE_RUST_WORKSPACE: "crates/ironmem/src/hook.rs",
    hook.SURFACE_COLLAB_PROTOCOL: "docs/COLLAB.md",
    hook.SURFACE_HOOK_SELF_TEST: "scripts/run_git_hook.py",
    hook.SURFACE_DOCS: "README.md",
}


@pytest.mark.parametrize("gate", hook.GATES, ids=[gate.name for gate in hook.GATES])
def test_resolve_gates_reaches_every_manifest_gate(gate):
    phase = next(iter(gate.phases))
    if gate.always:
        changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    else:
        surface_id = next(iter(gate.surfaces))
        path = _SURFACE_EXAMPLE_PATH_FOR_TEST[surface_id]
        changes = hook.ChangeSet(paths=(path,), unknown=False, reason=None)
    result = hook.resolve_gates(phase, changes)
    assert gate in result


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
