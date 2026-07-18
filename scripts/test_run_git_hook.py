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
import os
import pathlib
import subprocess
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


# --- Task 3: resolve_gates -- the `gate.always` disjunct, exercised for real
#
# No gate in today's manifest sets always=True (see
# test_manifest_no_gate_marked_always_yet), so the assertion above only ever
# proves the degenerate "() == ()" case: it can't distinguish "always works"
# from "always is unreachable dead code". Inject a synthetic always-gate via
# monkeypatch (same technique as
# test_resolve_gates_dedupes_by_first_seen_not_by_sorting) to cover the
# branch with a gate that actually sets always=True.


def test_resolve_gates_gate_always_true_runs_with_empty_paths(monkeypatch):
    always_gate = hook.Gate(
        name="synthetic_always_gate",
        argv=("true",),
        phases=frozenset({hook.PHASE_PRE_COMMIT}),
        surfaces=frozenset(),
        always=True,
    )
    monkeypatch.setattr(hook, "GATES", hook.GATES + (always_gate,))
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    # Empty paths, unknown=False: nothing escalates. Only the always=True
    # gate fires -- every real manifest gate (always=False) is excluded.
    assert hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes) == (always_gate,)


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


def test_resolve_gates_dedupes_by_first_seen_not_by_sorting(monkeypatch):
    # Fixture-based monkeypatch (auto-restores on teardown): assert on
    # classify_path call order via a spy that wraps the real function,
    # proving resolve_gates visits each distinct path exactly once, in
    # first-seen order -- never a sorted order, which would reorder
    # "notes_b.txt" before "notes_a.txt".
    calls: list[str] = []
    real_classify_path = hook.classify_path

    def spy(path):
        calls.append(path)
        return real_classify_path(path)

    changes = hook.ChangeSet(
        paths=("notes_b.txt", "notes_a.txt", "notes_b.txt", "notes_a.txt"),
        unknown=False,
        reason=None,
    )
    monkeypatch.setattr(hook, "classify_path", spy)
    hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)

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


# --- Task 3: resolve_gates -- parametrized per-gate-per-phase reachability -
#
# Derived from GATES itself, not a hardcoded list of gate names: a future
# gate appended to the manifest without an entry in
# _SURFACE_EXAMPLE_PATH_FOR_TEST below fails with a KeyError right here,
# rather than silently going unexercised.
#
# Parametrized over (gate, phase) pairs, not just gates: gate.phases is a
# frozenset, and CPython randomizes string hashing per process, so iterating
# a single `next(iter(gate.phases))` would exercise a different phase on
# different runs -- never both, never reproducibly, for any gate declaring
# more than one phase. Expanding to every declared pair removes that
# nondeterminism and strengthens the property being checked: every declared
# (gate, phase) pair must be reachable, not just one arbitrarily-chosen
# phase per gate. `sorted(gate.phases)` here is for deterministic *test
# parametrization ids*, not runtime gate ordering -- GATES itself is never
# sorted.
#
# SURFACE_DOCS is deliberately absent from the map below: no gate declares
# it today, and DOCS is defined as inert (see
# test_resolve_gates_docs_only_selects_no_gates). If a future gate declared
# SURFACE_DOCS, this map must raise KeyError naming that gate immediately,
# not silently resolve to a docs-classified path that would make the
# always/escalate-only property look satisfied when it isn't.
_SURFACE_EXAMPLE_PATH_FOR_TEST = {
    hook.SURFACE_RUST_WORKSPACE: "crates/ironmem/src/hook.rs",
    hook.SURFACE_COLLAB_PROTOCOL: "docs/COLLAB.md",
    hook.SURFACE_HOOK_SELF_TEST: "scripts/run_git_hook.py",
}

_GATE_PHASE_PARAMS_FOR_TEST = [
    (gate, phase) for gate in hook.GATES for phase in sorted(gate.phases)
]


@pytest.mark.parametrize(
    "gate, phase",
    _GATE_PHASE_PARAMS_FOR_TEST,
    ids=[f"{gate.name}-{phase}" for gate, phase in _GATE_PHASE_PARAMS_FOR_TEST],
)
def test_resolve_gates_reaches_every_manifest_gate(gate, phase):
    if gate.always:
        changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
        assert gate in hook.resolve_gates(phase, changes)
        return
    # Iterate every declared surface, not just one: a future gate declaring
    # two surfaces -- one mapped here, one not -- must raise KeyError
    # unconditionally (not on a hash-order coin flip), and must be proven
    # reachable from each surface it declares, not just an arbitrary one.
    for surface_id in gate.surfaces:
        path = _SURFACE_EXAMPLE_PATH_FOR_TEST[surface_id]
        changes = hook.ChangeSet(paths=(path,), unknown=False, reason=None)
        assert gate in hook.resolve_gates(phase, changes)


# --- Task 4: collection layer -- Git to ChangeSet, fail-closed ------------
#
# No test in this section invokes real Git: every `subprocess.run` call is
# replaced by `_FakeGitRun`, keyed on the exact argv tuple that follows
# "git". An unanticipated call raises KeyError (a loud test failure), which
# doubles as proof that each test drives an exact, intentional sequence of
# Git invocations -- never a superset or a silently-skipped one.

SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40


class _FakeGitRun:
    """Stand-in for `subprocess.run` used by every Task 4 test. Never shells
    out. `responses` maps an argv tuple (everything after "git") to either
    an `(returncode, stdout)` pair or an exception instance to raise,
    simulating a genuine subprocess-level failure (git missing, output
    undecodable, ...).
    """

    def __init__(self, responses):
        self.responses = responses
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, cmd, **kwargs):
        assert cmd[0] == "git"
        assert kwargs.get("cwd") == hook.ROOT
        assert kwargs.get("text") is True
        assert kwargs.get("capture_output") is True
        assert kwargs.get("check") is False
        assert "shell" not in kwargs
        args = tuple(cmd[1:])
        self.calls.append(args)
        outcome = self.responses[args]
        if isinstance(outcome, BaseException):
            raise outcome
        returncode, stdout = outcome
        return subprocess.CompletedProcess(cmd, returncode, stdout=stdout, stderr="")


def _pre_push_line(local_ref, local_sha, remote_ref, remote_sha):
    return f"{local_ref} {local_sha} {remote_ref} {remote_sha}"


# --- _split_nul -- byte-exact NUL-field splitting --------------------------


def test_split_nul_empty_output_is_no_paths():
    assert hook._split_nul("") == ()


def test_split_nul_drops_only_trailing_empty_field():
    assert hook._split_nul("a.py\0b.py\0") == ("a.py", "b.py")


def test_split_nul_preserves_interior_bytes_no_strip():
    # Leading/trailing whitespace and an embedded newline inside a field
    # must survive untouched -- -z framing removal is not path mutation.
    assert hook._split_nul(" a.py \0b\n.py\0") == (" a.py ", "b\n.py")


# --- _is_hex_sha -- sha validation (not a path, .strip()/case rules N/A) --


def test_is_hex_sha_accepts_full_hex_sha():
    assert hook._is_hex_sha(SHA_A) is True


def test_is_hex_sha_rejects_empty_string():
    assert hook._is_hex_sha("") is False


def test_is_hex_sha_rejects_non_hex_characters():
    assert hook._is_hex_sha("z" * 40) is False


def test_is_hex_sha_rejects_short_hex_run():
    # "abc" is hex-shaped but far shorter than a real Git object id (40 or
    # 64 hex chars). Accepting any positive-length hex run would make this
    # guard a formality rather than a load-bearing malformed-stdin check.
    assert hook._is_hex_sha("abc") is False


def test_is_hex_sha_accepts_sha256_length():
    assert hook._is_hex_sha("a" * 64) is True


def test_is_hex_sha_rejects_length_between_40_and_64():
    assert hook._is_hex_sha("a" * 50) is False


# --- _parse_pre_push_line ---------------------------------------------------


def test_parse_pre_push_line_valid_four_fields():
    line = _pre_push_line("refs/heads/a", SHA_A, "refs/heads/a", SHA_B)
    assert hook._parse_pre_push_line(line) == ("refs/heads/a", SHA_A, "refs/heads/a", SHA_B)


def test_parse_pre_push_line_wrong_field_count_is_none():
    assert hook._parse_pre_push_line("refs/heads/a onlytwo") is None


# --- _run_git -- fail-closed boundary itself must never raise --------------


def test_run_git_guards_empty_args_on_subprocess_failure(monkeypatch):
    # `_run_git(())` -- an empty args tuple -- must not raise IndexError from
    # inside its own except-block while building `reason`; that would let an
    # exception escape the one boundary that exists to convert Git
    # subprocess failures into a structured, non-raising signal.
    def raiser(cmd, **kwargs):
        raise OSError("boom")

    monkeypatch.setattr(hook.subprocess, "run", raiser)
    ok, returncode, stdout, reason = hook._run_git(())
    assert ok is False
    assert returncode == -1
    assert stdout == ""
    assert reason
    assert "boom" not in reason


# --- collect_pre_commit_changes ---------------------------------------------


def test_collect_pre_commit_changes_success(monkeypatch):
    fake = _FakeGitRun({("diff", "--cached", "--name-only", "-z"): (0, "a.py\0b/c.txt\0")})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_commit_changes()
    assert changes == hook.ChangeSet(paths=("a.py", "b/c.txt"), unknown=False, reason=None)
    assert fake.calls == [("diff", "--cached", "--name-only", "-z")]


def test_collect_pre_commit_changes_no_staged_files_is_not_unknown(monkeypatch):
    fake = _FakeGitRun({("diff", "--cached", "--name-only", "-z"): (0, "")})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_commit_changes()
    # Empty paths + unknown=False must mean "genuinely no changes", never
    # "collection broke".
    assert changes == hook.ChangeSet(paths=(), unknown=False, reason=None)


def test_collect_pre_commit_changes_nonzero_exit_is_unknown(monkeypatch):
    fake = _FakeGitRun({("diff", "--cached", "--name-only", "-z"): (128, "")})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_commit_changes()
    assert changes.paths == ()
    assert changes.unknown is True
    assert changes.reason


def test_collect_pre_commit_changes_subprocess_failure_is_unknown_never_raises(monkeypatch):
    fake = _FakeGitRun(
        {("diff", "--cached", "--name-only", "-z"): FileNotFoundError("git: command not found")}
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_commit_changes()
    assert changes.paths == ()
    assert changes.unknown is True
    assert changes.reason
    # The raw exception message is never propagated into `reason`.
    assert "command not found" not in changes.reason


def test_collect_pre_commit_changes_preserves_byte_exact_paths(monkeypatch):
    weird = "docs/plan\n notes (β).md"
    fake = _FakeGitRun({("diff", "--cached", "--name-only", "-z"): (0, f"{weird}\0")})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_commit_changes()
    assert changes.paths == (weird,)


def test_collect_pre_commit_changes_no_diff_filter_flag(monkeypatch):
    # Pins that collect_pre_commit_changes() never passes --diff-filter: the
    # exact argv it must invoke is asserted by _FakeGitRun's KeyError-on-
    # unanticipated-call behavior (see class docstring above) -- any
    # additional flag, including a reintroduced --diff-filter, would make
    # this call miss the fake's response table and fail loudly.
    fake = _FakeGitRun(
        {("diff", "--cached", "--name-only", "-z"): (0, "crates/ironmem/src/deleted.rs\0")}
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    hook.collect_pre_commit_changes()
    assert fake.calls == [("diff", "--cached", "--name-only", "-z")]


def test_staged_deletion_of_rust_path_is_collected_and_selects_rust_gates(monkeypatch):
    # The human-ratified behavior change: staged deletions reach gate
    # selection because --diff-filter=ACMRTUXB was deliberately dropped (see
    # the comment at collect_pre_commit_changes()). `git diff --name-only`
    # reports a deleted path exactly like any other changed path -- there is
    # no separate "deleted" marker in `-z --name-only` output -- so a fake
    # response containing only the path is a faithful stand-in for a staged
    # deletion of that path. This pins the ratified behavior: the deletion
    # is collected (not dropped) and it selects the Rust gates for
    # pre-commit.
    fake = _FakeGitRun(
        {("diff", "--cached", "--name-only", "-z"): (0, "crates/ironmem/src/deleted.rs\0")}
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_commit_changes()
    assert changes == hook.ChangeSet(
        paths=("crates/ironmem/src/deleted.rs",), unknown=False, reason=None
    )
    names = [gate.name for gate in hook.resolve_gates(hook.PHASE_PRE_COMMIT, changes)]
    assert "rust_fmt_check" in names
    assert "rust_clippy" in names


# --- collect_pre_push_changes -- happy paths --------------------------------


def test_collect_pre_push_changes_single_update(monkeypatch):
    stdin = _pre_push_line("refs/heads/feature", SHA_B, "refs/heads/feature", SHA_A) + "\n"
    fake = _FakeGitRun({("diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): (0, "x.py\0y.py\0")})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    assert changes == hook.ChangeSet(paths=("x.py", "y.py"), unknown=False, reason=None)


def test_collect_pre_push_changes_multi_ref_dedupes_first_seen(monkeypatch):
    stdin = (
        "\n".join(
            [
                _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A),
                _pre_push_line("refs/heads/b", SHA_C, "refs/heads/b", SHA_A),
            ]
        )
        + "\n"
    )
    fake = _FakeGitRun(
        {
            ("diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): (0, "x.py\0shared.py\0"),
            ("diff", "--name-only", "-z", f"{SHA_A}..{SHA_C}"): (0, "shared.py\0z.py\0"),
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    assert changes.paths == ("x.py", "shared.py", "z.py")
    assert changes.unknown is False


def test_collect_pre_push_changes_skips_deletion_ref(monkeypatch):
    stdin = _pre_push_line("refs/heads/gone", hook.ZERO_SHA, "refs/heads/gone", SHA_A) + "\n"
    fake = _FakeGitRun({})  # no git diff call should happen at all
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    assert changes == hook.ChangeSet(paths=(), unknown=False, reason=None)
    assert fake.calls == []


def test_collect_pre_push_changes_branch_creation_uses_default_base(monkeypatch):
    stdin = _pre_push_line("refs/heads/new", SHA_B, "refs/heads/new", hook.ZERO_SHA) + "\n"
    fake = _FakeGitRun(
        {
            ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"): (
                0,
                "refs/remotes/origin/main\n",
            ),
            ("merge-base", SHA_B, "refs/remotes/origin/main"): (0, SHA_A + "\n"),
            ("diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): (0, "new_file.py\0"),
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    assert changes == hook.ChangeSet(paths=("new_file.py",), unknown=False, reason=None)


def test_collect_pre_push_changes_missing_upstream_falls_back_to_root_diff(monkeypatch):
    stdin = _pre_push_line("refs/heads/new", SHA_B, "refs/heads/new", hook.ZERO_SHA) + "\n"
    fake = _FakeGitRun(
        {
            ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"): (1, ""),
            ("merge-base", SHA_B, "origin/main"): (1, ""),
            ("merge-base", SHA_B, "origin/master"): (1, ""),
            ("merge-base", SHA_B, "main"): (1, ""),
            ("merge-base", SHA_B, "master"): (1, ""),
            ("diff-tree", "--root", "--no-commit-id", "--name-only", "-z", "-r", SHA_B): (
                0,
                "root.py\0",
            ),
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    assert changes == hook.ChangeSet(paths=("root.py",), unknown=False, reason=None)


def test_collect_pre_push_changes_empty_stdin_is_not_unknown(monkeypatch):
    fake = _FakeGitRun({})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes("")
    assert changes == hook.ChangeSet(paths=(), unknown=False, reason=None)
    assert fake.calls == []


# --- collect_pre_push_changes -- fail-closed: Git failures ------------------


def test_collect_pre_push_changes_git_failure_mid_batch_preserves_prior_paths(monkeypatch):
    stdin = (
        "\n".join(
            [
                _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A),
                _pre_push_line("refs/heads/b", SHA_C, "refs/heads/b", SHA_A),
            ]
        )
        + "\n"
    )
    fake = _FakeGitRun(
        {
            ("diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): (0, "x.py\0"),
            ("diff", "--name-only", "-z", f"{SHA_A}..{SHA_C}"): (128, ""),
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    # Whatever was collected before the failure is preserved -- never wiped
    # back to an empty tuple.
    assert changes.paths == ("x.py",)
    assert changes.unknown is True
    assert changes.reason


def test_collect_pre_push_changes_subprocess_failure_is_unknown_never_raises(monkeypatch):
    stdin = _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A) + "\n"
    fake = _FakeGitRun({("diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): OSError("boom")})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    assert changes.paths == ()
    assert changes.unknown is True
    assert changes.reason
    assert "boom" not in changes.reason


# --- collect_pre_push_changes -- fail-closed: malformed stdin ---------------


@pytest.mark.parametrize(
    "stdin",
    [
        "refs/heads/a onlythreefields\n",
        f"refs/heads/a zzzznothex refs/heads/a {SHA_A}\n",
        f"refs/heads/a {SHA_A} refs/heads/a zzzznothex\n",
        "☠️ not a valid pre-push line at all\n",
    ],
    ids=["wrong-field-count", "non-hex-local-sha", "non-hex-remote-sha", "junk"],
)
def test_collect_pre_push_changes_malformed_stdin_is_unknown_never_raises(monkeypatch, stdin):
    fake = _FakeGitRun({})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.collect_pre_push_changes(stdin)
    assert changes.unknown is True
    assert changes.reason
    # Malformed input is rejected before any Git call is attempted.
    assert fake.calls == []


class _StdinStub:
    """Minimal stand-in for sys.stdin exposing only the .read() that
    main("pre-push") calls -- avoids touching the real process stdin in
    tests.
    """

    def __init__(self, text: str) -> None:
        self._text = text

    def read(self) -> str:
        return self._text


# --- Task 6: legacy retirement -- these symbols must be gone, not merely
# unused --------------------------------------------------------------------


def test_legacy_pre_task6_functions_are_removed():
    for name in (
        "run_pre_commit",
        "run_pre_push",
        "gate_summary",
        "run",
        "staged_paths",
        "pushed_paths",
    ):
        assert not hasattr(hook, name), f"{name} should have been retired in Task 6"


# --- Task 5: execution layer -- hardened subprocess contract --------------
#
# No test in this section invokes a real gate command (cargo/pytest/bash are
# slow and side-effecting). Every `subprocess.run` call is replaced by
# `_FakeGateRun`, keyed on the exact argv *list* execute_gates passes --
# distinct from `_FakeGitRun` above, which asserts `cmd[0] == "git"` and
# would reject a gate invocation outright.


class _FakeGateRun:
    """Stand-in for `subprocess.run` used by every Task 5 test. Never shells
    out and never runs a real gate. `responses` maps an argv tuple to either
    the returncode that call should yield, or an exception instance to raise
    (simulating exec-time failures such as a missing gate binary). An
    unanticipated call raises KeyError, proving each test drives an exact,
    intentional sequence of gate invocations.
    """

    def __init__(self, responses):
        self.responses = responses
        self.calls: list[tuple[list, dict]] = []

    def __call__(self, cmd, **kwargs):
        self.calls.append((list(cmd), kwargs))
        key = tuple(cmd)
        outcome = self.responses[key]
        if isinstance(outcome, BaseException):
            raise outcome
        return subprocess.CompletedProcess(cmd, outcome)


def _only_rust_changes():
    return hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )


# --- execute_gates -- runs only resolver-selected gates, in manifest order -


def test_execute_gates_runs_only_selected_gates_in_manifest_order(monkeypatch):
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): 0,
            (
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ): 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    rc = hook.execute_gates(hook.PHASE_PRE_COMMIT, _only_rust_changes())
    assert rc == 0
    assert [cmd for cmd, _kwargs in fake.calls] == [
        ["cargo", "fmt", "--all", "--", "--check"],
        [
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    ]


def test_execute_gates_returns_zero_when_nothing_selected(monkeypatch):
    fake = _FakeGateRun({})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.ChangeSet(paths=("README.md",), unknown=False, reason=None)
    rc = hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    assert rc == 0
    assert fake.calls == []


# --- execute_gates -- the hardened subprocess contract itself -------------


def test_execute_gates_calls_subprocess_run_with_exact_contract(monkeypatch):
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): 0,
            (
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ): 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    hook.execute_gates(hook.PHASE_PRE_COMMIT, _only_rust_changes())
    cmd, kwargs = fake.calls[0]
    assert cmd == ["cargo", "fmt", "--all", "--", "--check"]
    assert kwargs.get("shell") is False
    assert kwargs.get("cwd") == hook.ROOT
    assert kwargs.get("check") is False
    assert "env" in kwargs


# --- execute_gates -- stop-at-first-failure, propagate exact exit code ----


def test_execute_gates_stops_at_first_failure_and_propagates_exit_code(monkeypatch):
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): 3,
            (
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ): 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    rc = hook.execute_gates(hook.PHASE_PRE_COMMIT, _only_rust_changes())
    assert rc == 3
    # rust_clippy must never be invoked once rust_fmt_check has failed.
    assert [cmd for cmd, _kwargs in fake.calls] == [
        ["cargo", "fmt", "--all", "--", "--check"]
    ]


# --- execute_gates -- one deterministic line per gate: run/skip/fail ------


def test_execute_gates_prints_run_line_for_selected_gate(monkeypatch, capsys):
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): 0})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    # Restrict to a single-gate manifest slice so this test only proves the
    # run-line, not interactions with the rest of the real manifest.
    fmt_gate = next(gate for gate in hook.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(hook, "GATES", (fmt_gate,))
    hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    # Exact line, not a substring check -- the task calls this format
    # deterministic, so the test should pin the literal bytes rather than
    # accept e.g. "rerun" or a stray "7" anywhere in unrelated output.
    assert out == "[git-hook] rust_fmt_check: run\n"


def test_execute_gates_prints_skip_line_with_surfaces_not_touched(monkeypatch, capsys):
    fake = _FakeGateRun({})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    collab_gate = next(
        gate for gate in hook.GATES if gate.name == "collab_template_lint"
    )
    monkeypatch.setattr(hook, "GATES", (collab_gate,))
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "collab_template_lint" in out
    assert "skip" in out
    assert hook.SURFACE_COLLAB_PROTOCOL in out
    assert fake.calls == []


def test_execute_gates_skip_line_lists_surfaces_deterministically(monkeypatch, capsys):
    # NOTE: kept for its original within-process-rerun angle, but every gate
    # in today's manifest (including hook_self_test, used here) declares
    # exactly one surface, so this test cannot actually distinguish ordered
    # output from raw frozenset iteration -- with one element the two are
    # byte-identical by construction. It would pass identically against the
    # bug it was meant to catch. The real ordering-property coverage is
    # test__ordered_surfaces_returns_declaration_order below, which drives
    # _ordered_surfaces directly with a genuinely multi-element frozenset.
    fake = _FakeGateRun({})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    hook_gate = next(gate for gate in hook.GATES if gate.name == "hook_self_test")
    monkeypatch.setattr(hook, "GATES", (hook_gate,))
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    first = capsys.readouterr().out
    hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    second = capsys.readouterr().out
    assert first == second


# --- _ordered_surfaces -- the real ordering-property coverage -------------


def test__ordered_surfaces_returns_declaration_order():
    # SURFACES declares SURFACE_RUST_WORKSPACE before SURFACE_COLLAB_PROTOCOL
    # before SURFACE_HOOK_SELF_TEST before SURFACE_DOCS (see the SURFACES
    # MappingProxyType above). Passing the two later-declared ids in reverse
    # frozenset-construction order and asserting the exact declaration-order
    # tuple back is the property test_execute_gates_skip_line_lists_
    # surfaces_deterministically cannot provide with a single-element input:
    # this fails if _ordered_surfaces ever regresses to raw frozenset
    # iteration (hash-randomized, not declaration order).
    surface_ids = frozenset({hook.SURFACE_DOCS, hook.SURFACE_HOOK_SELF_TEST})
    assert hook._ordered_surfaces(surface_ids) == (
        hook.SURFACE_HOOK_SELF_TEST,
        hook.SURFACE_DOCS,
    )

    all_surfaces = frozenset(
        {
            hook.SURFACE_DOCS,
            hook.SURFACE_COLLAB_PROTOCOL,
            hook.SURFACE_HOOK_SELF_TEST,
            hook.SURFACE_RUST_WORKSPACE,
        }
    )
    assert hook._ordered_surfaces(all_surfaces) == (
        hook.SURFACE_RUST_WORKSPACE,
        hook.SURFACE_COLLAB_PROTOCOL,
        hook.SURFACE_HOOK_SELF_TEST,
        hook.SURFACE_DOCS,
    )


def test_execute_gates_prints_fail_line_with_exit_code(monkeypatch, capsys):
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): 7})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    fmt_gate = next(gate for gate in hook.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(hook, "GATES", (fmt_gate,))
    changes = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    rc = hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    assert rc == 7
    out = capsys.readouterr().out
    assert "rust_fmt_check" in out
    assert "fail" in out
    assert "7" in out


def test_execute_gates_normalizes_negative_returncode_from_signal_kill(monkeypatch, capsys):
    # A signal-killed gate (e.g. SIGKILL) reports a negative returncode from
    # subprocess.run. Pin the shell-convention normalization (128 + signal)
    # so a downstream `sys.exit(code)` can't land on the wrong exit status
    # via Python's exit-code modulo (sys.exit(-9) -> 247, not -9).
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): -9})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    fmt_gate = next(gate for gate in hook.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(hook, "GATES", (fmt_gate,))
    changes = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    rc = hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    assert rc == 137
    out = capsys.readouterr().out
    assert out == "[git-hook] rust_fmt_check: run\n[git-hook] rust_fmt_check: fail (137)\n"


def test_execute_gates_missing_gate_binary_prints_fail_line_then_raises(monkeypatch, capsys):
    # A gate binary absent from PATH (e.g. no `cargo` installed) makes
    # subprocess.run raise FileNotFoundError before any CompletedProcess
    # exists. This must not silently traceback with no fail(...) line --
    # the one-line-per-gate contract still applies, then the exception
    # propagates (fail-loud: a broken environment, not a recoverable gate
    # failure).
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): FileNotFoundError(
                2, "No such file or directory", "cargo"
            )
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    fmt_gate = next(gate for gate in hook.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(hook, "GATES", (fmt_gate,))
    changes = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    with pytest.raises(FileNotFoundError):
        hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "rust_fmt_check: fail" in out


# --- execute_gates -- unknown=True prints the escalation reason -----------


def test_execute_gates_prints_escalation_reason_when_unknown(monkeypatch, capsys):
    fake = _FakeGateRun({("python3", "scripts/test_run_git_hook.py"): 0})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    hook_gate = next(gate for gate in hook.GATES if gate.name == "hook_self_test")
    monkeypatch.setattr(hook, "GATES", (hook_gate,))
    changes = hook.ChangeSet(paths=(), unknown=True, reason="git diff failed mid-batch")
    hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "git diff failed mid-batch" in out


def test_execute_gates_no_escalation_line_when_known(monkeypatch, capsys):
    fake = _FakeGateRun({})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "escalat" not in out.lower()


def test_execute_gates_unknown_true_reason_none_prints_no_escalation_line_but_still_escalates(
    monkeypatch, capsys
):
    # Pins the previously-untested `unknown=True, reason=None` combination:
    # `if changes.unknown and changes.reason:` at the top of execute_gates
    # means no escalation line prints when reason is falsy, even though
    # resolve_gates still escalates to running every phase-matching gate on
    # `changes.unknown` alone (independent of `reason`). Both halves of that
    # behavior are asserted here so a future change can't silently drop
    # either the guard or the escalation.
    fake = _FakeGateRun({("python3", "scripts/test_run_git_hook.py"): 0})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    hook_gate = next(gate for gate in hook.GATES if gate.name == "hook_self_test")
    monkeypatch.setattr(hook, "GATES", (hook_gate,))
    changes = hook.ChangeSet(paths=(), unknown=True, reason=None)
    rc = hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "escalat" not in out.lower()
    # The gate still ran despite the silent escalation -- unknown=True alone
    # is enough for resolve_gates to select it.
    assert rc == 0
    assert [cmd for cmd, _kwargs in fake.calls] == [
        ["python3", "scripts/test_run_git_hook.py"]
    ]


# --- execute_gates -- invalid phase still fails loudly (delegates to
# resolve_gates's own guard, not re-implemented) ----------------------------


def test_execute_gates_unknown_phase_raises():
    changes = hook.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(ValueError):
        hook.execute_gates("typo-phase", changes)


# --- _scrub_git_env -- the security-critical part of this task ------------
#
# This is the direct regression guard for PR #186: a pre-push hook exporting
# GIT_DIR/GIT_INDEX_FILE/GIT_WORK_TREE, inherited by a `cargo test` tempdir
# Git fixture, committed junk onto the real branch. These tests assert on
# the env dict itself, not a call count.


def test_scrub_git_env_strips_repo_redirecting_vars():
    source = {
        "GIT_DIR": "/tmp/evil/.git",
        "GIT_INDEX_FILE": "/tmp/evil/index",
        "GIT_WORK_TREE": "/tmp/evil",
        "GIT_OBJECT_DIRECTORY": "/tmp/evil/objects",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES": "/tmp/evil/alt",
        "GIT_COMMON_DIR": "/tmp/evil/common",
        "GIT_NAMESPACE": "evil-namespace",
        "PATH": "/usr/bin",
    }
    scrubbed = hook._scrub_git_env(source)
    for dangerous_key in (
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ):
        assert dangerous_key not in scrubbed
    assert scrubbed["PATH"] == "/usr/bin"


def test_scrub_git_env_keeps_explicit_keep_list():
    # NOTE: this test previously also asserted GIT_CONFIG_COUNT/
    # GIT_CONFIG_KEY_0/GIT_CONFIG_VALUE_0 were kept. That was a genuine spec
    # bug, not a preference: GIT_CONFIG_* is the documented equivalent of
    # `git -c <key>=<value>` for arbitrary config, including core.worktree --
    # the config equivalent of GIT_WORK_TREE, which this same module strips
    # as a repo-redirecting variable a few lines above. Keeping GIT_CONFIG_*
    # let GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.worktree
    # GIT_CONFIG_VALUE_0=/real/repo reproduce the exact PR #186 redirection
    # this scrub exists to prevent. The contract changed to strip
    # GIT_CONFIG_*; see test_scrub_git_env_strips_git_config_star below for
    # the corrected behavior.
    source = {
        "GIT_ASKPASS": "/usr/bin/askpass",
        "GIT_SSH": "/usr/bin/ssh",
        "GIT_SSH_COMMAND": "ssh -i key",
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_TRACE": "1",
        "GIT_TRACE_PACKET": "1",
    }
    scrubbed = hook._scrub_git_env(source)
    assert scrubbed == source


def test_scrub_git_env_strips_git_config_star():
    # Regression guard for the GIT_CONFIG_* keep-list bug: GIT_CONFIG_COUNT +
    # GIT_CONFIG_KEY_n/GIT_CONFIG_VALUE_n are the documented equivalent of
    # `git -c <key>=<value>` -- arbitrary config, including core.worktree
    # (the config equivalent of GIT_WORK_TREE). GIT_CONFIG_GLOBAL/
    # GIT_CONFIG_SYSTEM (also GIT_CONFIG_-prefixed) replace whole config
    # files. All must be stripped like any other unrecognized GIT_* var.
    source = {
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "core.worktree",
        "GIT_CONFIG_VALUE_0": "/real/repo",
        "GIT_CONFIG_GLOBAL": "/tmp/evil/gitconfig",
        "GIT_CONFIG_SYSTEM": "/tmp/evil/gitconfig",
        "PATH": "/usr/bin",
    }
    scrubbed = hook._scrub_git_env(source)
    for dangerous_key in (
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
    ):
        assert dangerous_key not in scrubbed
    assert scrubbed["PATH"] == "/usr/bin"


def test_scrub_git_env_strips_unlisted_git_var_not_yet_enumerated():
    # Default-toward-scrubbing: an unrecognized GIT_* variable not in the
    # keep-list must be dropped, not silently passed through.
    source = {"GIT_SOME_FUTURE_FLAG": "danger", "PATH": "/usr/bin"}
    scrubbed = hook._scrub_git_env(source)
    assert "GIT_SOME_FUTURE_FLAG" not in scrubbed
    assert scrubbed["PATH"] == "/usr/bin"


def test_scrub_git_env_returns_new_dict_never_mutates_source():
    source = {"GIT_DIR": "/tmp/evil", "PATH": "/usr/bin"}
    original = dict(source)
    hook._scrub_git_env(source)
    assert source == original


# --- execute_gates -- env scrub wired end-to-end into the subprocess call -


def test_execute_gates_scrubs_git_env_before_running_a_gate(monkeypatch):
    monkeypatch.setenv("GIT_DIR", "/tmp/evil/.git")
    monkeypatch.setenv("GIT_INDEX_FILE", "/tmp/evil/index")
    monkeypatch.setenv("GIT_WORK_TREE", "/tmp/evil")
    monkeypatch.setenv("GIT_ASKPASS", "/usr/bin/askpass")
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): 0})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    fmt_gate = next(gate for gate in hook.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(hook, "GATES", (fmt_gate,))
    changes = hook.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    hook.execute_gates(hook.PHASE_PRE_COMMIT, changes)
    _cmd, kwargs = fake.calls[0]
    child_env = kwargs["env"]
    assert "GIT_DIR" not in child_env
    assert "GIT_INDEX_FILE" not in child_env
    assert "GIT_WORK_TREE" not in child_env
    assert child_env.get("GIT_ASKPASS") == "/usr/bin/askpass"
    # Non-Git variables inherited from the real environment must survive.
    assert child_env.get("PATH") == os.environ.get("PATH")


# --- Task 6: main(phase) -- collect -> resolve -> execute, wired end-to-end
#
# No test in this section invokes real Git or a real gate command. Every
# `subprocess.run` call goes through `_FakeSubprocessRun` below: unlike
# `_FakeGitRun` (asserts `cmd[0] == "git"`) or `_FakeGateRun` (keyed only on
# gate argv), `main()` drives both kinds of call through the same
# `subprocess.run` seam in one test, so this fake is keyed on the exact full
# argv tuple regardless of program name. An unanticipated call raises
# KeyError, proving each test drives an exact, intentional call sequence.


class _FakeSubprocessRun:
    """Stand-in for `subprocess.run` used by every Task 6 `main()`/
    `_cli_main()` test. `responses` maps a full argv tuple to either
    `(returncode, stdout)` (the shape Git collection calls need), a bare
    `int` returncode (the shape gate calls need -- `execute_gates` never
    reads `stdout`), or an exception instance to raise.
    """

    def __init__(self, responses):
        self.responses = responses
        self.calls: list[list[str]] = []

    def __call__(self, cmd, **kwargs):
        self.calls.append(list(cmd))
        outcome = self.responses[tuple(cmd)]
        if isinstance(outcome, BaseException):
            raise outcome
        if isinstance(outcome, tuple):
            returncode, stdout = outcome
            return subprocess.CompletedProcess(cmd, returncode, stdout=stdout, stderr="")
        return subprocess.CompletedProcess(cmd, outcome)


_RUST_FMT_ARGV = ("cargo", "fmt", "--all", "--", "--check")
_RUST_CLIPPY_ARGV = (
    "cargo",
    "clippy",
    "--workspace",
    "--all-targets",
    "--all-features",
    "--",
    "-D",
    "warnings",
)
_RUST_TEST_ARGV = ("cargo", "test", "--workspace")
_HOOK_SELF_TEST_ARGV = ("python3", "scripts/test_run_git_hook.py")
_HOOK_INSTALL_CHECK_ARGV = ("bash", "scripts/install-git-hooks.sh", "--check")
_COLLAB_LINT_ARGV = ("python3", "scripts/check_collab_turn_templates.py")


# --- main() -- end-to-end, one phase at a time: a Rust-only change selects
# exactly today's gate set for that phase (exact sequence, not a count) ----


def test_main_pre_commit_rust_only_change_runs_exact_gate_sequence(monkeypatch):
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--cached", "--name-only", "-z"): (
                0,
                "crates/ironmem/src/hook.rs\0",
            ),
            _RUST_FMT_ARGV: 0,
            _RUST_CLIPPY_ARGV: 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    rc = hook.main(hook.PHASE_PRE_COMMIT)
    assert rc == 0
    assert fake.calls == [
        ["git", "diff", "--cached", "--name-only", "-z"],
        list(_RUST_FMT_ARGV),
        list(_RUST_CLIPPY_ARGV),
    ]


def test_main_pre_push_rust_only_change_runs_exact_gate_sequence(monkeypatch):
    stdin = _pre_push_line("refs/heads/feature", SHA_B, "refs/heads/feature", SHA_A) + "\n"
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): (
                0,
                "crates/ironmem/src/hook.rs\0",
            ),
            _RUST_TEST_ARGV: 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(stdin))
    rc = hook.main(hook.PHASE_PRE_PUSH)
    assert rc == 0
    assert fake.calls == [
        ["git", "diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"],
        list(_RUST_TEST_ARGV),
    ]


# --- main() -- fail-closed property, now via collect -> execute_gates
# directly (no raising legacy adapter in between)
#
# Global constraint: "A Git failure must not let the hook exit 0 with zero
# gates run." Under this architecture that guarantee is upheld differently
# than the pre-Task-6 staged_paths()/pushed_paths() SystemExit: a Git
# failure sets ChangeSet.unknown=True, and resolve_gates (already wired,
# unchanged by this task) escalates unknown=True to select every
# phase-matching gate -- main() never special-cases this, it falls out
# directly from collect -> execute_gates. So the property proven here is
# "every phase gate is actually attempted, and its real result decides the
# exit code" -- never "silently present as success having run nothing".


def test_main_pre_commit_git_failure_escalates_to_every_gate_never_zero_gates_run(
    monkeypatch, capsys
):
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--cached", "--name-only", "-z"): (128, ""),
            _HOOK_SELF_TEST_ARGV: 0,
            _HOOK_INSTALL_CHECK_ARGV: 0,
            _COLLAB_LINT_ARGV: 0,
            _RUST_FMT_ARGV: 0,
            _RUST_CLIPPY_ARGV: 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    rc = hook.main(hook.PHASE_PRE_COMMIT)
    out = capsys.readouterr().out
    assert rc == 0
    assert "escalating" in out
    non_git_calls = [cmd for cmd in fake.calls if cmd[0] != "git"]
    # Every pre-commit gate was actually attempted -- the old defect ("no
    # staged files; skipping gates", exit 0, zero gates run) cannot recur: a
    # Git failure here forces the full phase gate set to run.
    assert non_git_calls == [
        list(_HOOK_SELF_TEST_ARGV),
        list(_HOOK_INSTALL_CHECK_ARGV),
        list(_COLLAB_LINT_ARGV),
        list(_RUST_FMT_ARGV),
        list(_RUST_CLIPPY_ARGV),
    ]


def test_main_pre_commit_git_failure_escalated_gate_failure_is_nonzero_exit(monkeypatch):
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--cached", "--name-only", "-z"): (128, ""),
            _HOOK_SELF_TEST_ARGV: 1,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    rc = hook.main(hook.PHASE_PRE_COMMIT)
    assert rc == 1


def test_main_pre_push_git_failure_escalates_to_every_gate(monkeypatch, capsys):
    stdin = _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A) + "\n"
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): (128, ""),
            _HOOK_SELF_TEST_ARGV: 0,
            _COLLAB_LINT_ARGV: 0,
            _RUST_TEST_ARGV: 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(stdin))
    rc = hook.main(hook.PHASE_PRE_PUSH)
    out = capsys.readouterr().out
    assert rc == 0
    assert "escalating" in out
    non_git_calls = [cmd for cmd in fake.calls if cmd[0] != "git"]
    assert non_git_calls == [
        list(_HOOK_SELF_TEST_ARGV),
        list(_COLLAB_LINT_ARGV),
        list(_RUST_TEST_ARGV),
    ]
    # The @{u} fallback must never fire once unknown=True -- escalation, not
    # the manual-invocation fallback, is the fail-closed path here.
    assert ("git", "rev-parse", "--verify", "@{u}") not in [tuple(c) for c in fake.calls]


def test_main_unknown_phase_raises_before_any_io(monkeypatch):
    def poison(cmd, **kwargs):
        raise AssertionError(f"unexpected subprocess.run call: {cmd}")

    monkeypatch.setattr(hook.subprocess, "run", poison)
    with pytest.raises(ValueError):
        hook.main("typo-phase")


# --- main("pre-push") -- @{u} fallback for manual/direct invocation -------
#
# DECISION (see task-6-report.md for the full rationale): kept, ported from
# the retired pushed_paths()'s `@{u}` fallback. The real `git push`-invoked
# hook always pipes ref-update lines to stdin, so collect_pre_push_changes
# never legitimately sees empty/no-line stdin in that path; this fallback
# exists solely for a developer running
# `python3 scripts/run_git_hook.py pre-push` directly with no piped stdin.
# Implemented in main() itself, not inside collect_pre_push_changes() --
# that function's empty-stdin contract (unknown=False, paths=(), no Git
# calls -- see test_collect_pre_push_changes_empty_stdin_is_not_unknown) is
# Task 4's already-tested collection-layer behavior and is not touched here.


def test_main_pre_push_manual_invocation_falls_back_to_upstream(monkeypatch):
    fake = _FakeSubprocessRun(
        {
            ("git", "rev-parse", "--verify", "@{u}"): (0, SHA_A + "\n"),
            ("git", "diff", "--name-only", "-z", f"{SHA_A}..HEAD"): (
                0,
                "crates/ironmem/src/hook.rs\0",
            ),
            _RUST_TEST_ARGV: 0,
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(""))
    rc = hook.main(hook.PHASE_PRE_PUSH)
    assert rc == 0
    assert fake.calls == [
        ["git", "rev-parse", "--verify", "@{u}"],
        ["git", "diff", "--name-only", "-z", f"{SHA_A}..HEAD"],
        list(_RUST_TEST_ARGV),
    ]


def test_main_pre_push_manual_invocation_no_upstream_runs_no_gates(monkeypatch):
    fake = _FakeSubprocessRun({("git", "rev-parse", "--verify", "@{u}"): (128, "")})
    monkeypatch.setattr(hook.subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(""))
    rc = hook.main(hook.PHASE_PRE_PUSH)
    assert rc == 0
    assert fake.calls == [["git", "rev-parse", "--verify", "@{u}"]]


def test_main_pre_push_manual_invocation_upstream_diff_failure_raises_systemexit(monkeypatch):
    # Ported behavior: the fallback's diff call uses the legacy `git()`
    # helper at its default `check=True`, so a Git failure here aborts
    # immediately via SystemExit -- zero gates run, matching the literal
    # pre-Task-6 pushed_paths() `@{u}` fallback behavior (distinct from the
    # primary collection path above, which escalates instead of aborting).
    fake = _FakeSubprocessRun(
        {
            ("git", "rev-parse", "--verify", "@{u}"): (0, SHA_A + "\n"),
            ("git", "diff", "--name-only", "-z", f"{SHA_A}..HEAD"): (128, ""),
        }
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(""))
    with pytest.raises(SystemExit):
        hook.main(hook.PHASE_PRE_PUSH)


def test_main_pre_push_with_stdin_paths_never_triggers_upstream_fallback(monkeypatch):
    stdin = _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A) + "\n"
    fake = _FakeSubprocessRun(
        {("git", "diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"): (0, "README.md\0")}
    )
    monkeypatch.setattr(hook.subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(stdin))
    rc = hook.main(hook.PHASE_PRE_PUSH)
    assert rc == 0
    # Non-empty paths (even docs-only, which selects zero gates) must never
    # trigger the @{u} fallback -- it is reserved for the genuinely-empty
    # case only.
    assert fake.calls == [["git", "diff", "--name-only", "-z", f"{SHA_A}..{SHA_B}"]]


# --- _cli_main(argv) -- the usage-error / exit-2 contract, preserved from
# the pre-Task-6 main(argv) ---------------------------------------------
#
# No subprocess mocking needed for the invalid-argv cases: validation must
# reject before any collection or gate I/O is attempted.


def test_cli_main_missing_argument_prints_usage_and_returns_2(capsys):
    assert hook._cli_main(["scripts/run_git_hook.py"]) == 2
    err = capsys.readouterr().err
    assert err == "usage: scripts/run_git_hook.py <pre-commit|pre-push>\n"


def test_cli_main_bad_argument_prints_usage_and_returns_2(capsys):
    assert hook._cli_main(["scripts/run_git_hook.py", "typo-phase"]) == 2
    err = capsys.readouterr().err
    assert err == "usage: scripts/run_git_hook.py <pre-commit|pre-push>\n"


def test_cli_main_extra_arguments_prints_usage_and_returns_2(capsys):
    assert hook._cli_main(["scripts/run_git_hook.py", "pre-commit", "extra"]) == 2


def test_cli_main_valid_phase_delegates_to_main(monkeypatch):
    calls = []

    def fake_main(phase):
        calls.append(phase)
        return 0

    monkeypatch.setattr(hook, "main", fake_main)
    assert hook._cli_main(["scripts/run_git_hook.py", "pre-commit"]) == 0
    assert calls == ["pre-commit"]


def test_cli_main_propagates_main_exit_code(monkeypatch):
    monkeypatch.setattr(hook, "main", lambda phase: 3)
    assert hook._cli_main(["scripts/run_git_hook.py", "pre-push"]) == 3


# --- module-wide static guard -----------------------------------------------


def test_module_source_never_uses_shell_true():
    assert "shell=True" not in HOOK.read_text()


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
