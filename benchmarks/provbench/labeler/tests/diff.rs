use provbench_labeler::diff::{
    is_whitespace_or_comment_only, rename_candidate, rename_candidate_typed, RenameCandidate,
    RenameOrigin,
};
use provbench_labeler::lang::Language;
use std::collections::HashSet;
use std::path::Path;

#[test]
fn pure_whitespace_diff_is_ignored() {
    assert!(is_whitespace_or_comment_only(
        b"fn x()  {}",
        b"fn x() {}",
        Language::Rust
    ));
}

#[test]
fn comment_only_diff_is_ignored() {
    assert!(is_whitespace_or_comment_only(
        b"fn x() {} // a",
        b"fn x() {} // b",
        Language::Rust
    ));
}

#[test]
fn rename_is_not_whitespace_only() {
    assert!(!is_whitespace_or_comment_only(
        b"fn x() {}",
        b"fn y() {}",
        Language::Rust
    ));
}

// ── Task 13: Python-aware whitespace/comment detection ──────────────────────

#[test]
fn python_whitespace_only_detected() {
    let before = b"def f():\n    return 1\n";
    let after = b"def f():\n    return 1\n\n"; // trailing newline only
    assert!(is_whitespace_or_comment_only(
        before,
        after,
        Language::Python
    ));
}

#[test]
fn python_comment_only_detected() {
    let before = b"def f():\n    return 1  # ok\n";
    let after = b"def f():\n    return 1  # OK!\n";
    assert!(is_whitespace_or_comment_only(
        before,
        after,
        Language::Python
    ));
}

#[test]
fn python_body_change_not_trivial() {
    let before = b"def f():\n    return 1\n";
    let after = b"def f():\n    return 2\n";
    assert!(!is_whitespace_or_comment_only(
        before,
        after,
        Language::Python
    ));
}

#[test]
fn python_docstring_change_not_trivial() {
    // Python docstrings parse as `string` expressions — significant tokens,
    // NOT skipped like comments. Changing one is a real diff.
    let before = b"def f():\n    \"\"\"old doc\"\"\"\n    return 1\n";
    let after = b"def f():\n    \"\"\"new doc\"\"\"\n    return 1\n";
    assert!(!is_whitespace_or_comment_only(
        before,
        after,
        Language::Python
    ));
}

#[test]
fn rust_whitespace_only_still_works() {
    let before = b"fn f() -> u32 { 1 }\n";
    let after = b"fn  f()  ->  u32  {  1  }\n";
    assert!(is_whitespace_or_comment_only(before, after, Language::Rust));
}

#[test]
fn rust_comment_only_still_works() {
    let before = b"fn f() { /* old */ }\n";
    let after = b"fn f() { /* new */ }\n";
    assert!(is_whitespace_or_comment_only(before, after, Language::Rust));
}

#[test]
fn high_similarity_rename_is_detected() {
    let before = b"fn search_pattern(pat: &str) -> Vec<usize> { Vec::new() }";
    let after_candidates = vec![
        (
            "search_input".to_string(),
            b"fn search_input(pat: &str) -> Vec<usize> { Vec::new() }".to_vec(),
        ),
        (
            "totally_different".to_string(),
            b"fn totally_different() {}".to_vec(),
        ),
    ];
    let m = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(m.as_deref(), Some("search_input"));
}

#[test]
fn no_candidate_above_threshold_returns_none() {
    let before = b"fn alpha() {}";
    let after_candidates = vec![(
        "beta".to_string(),
        b"fn beta(x: u32, y: u32, z: u32) -> Vec<u32> { Vec::new() }".to_vec(),
    )];
    let m = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(m, None);
}

// ── Pass-3 hardening regressions (diff level) ────────────────────────────────
//
// HP3-3b and HP3-3 are RED against HEAD a6b7e5c (Cluster C rename false
// positives) and carry `#[ignore = "RED until Task 4 …"]` so that
// `cargo test` stays green.
// HP3-4 is GREEN on HEAD (a preservation contract for Task 4).
//
// HP3-2 (Cluster B per-commit symbol resolution) is an integration-level
// test in replay_hardening.rs.  The diff-level pin for the rename heuristic
// side of that scenario is here as HP3-3b (Cluster C, Task 4).

// ── HP3-3b: rename candidate triggers for replace_with_captures → replace_with_caps

/// `fn replace_with_captures(s: &str) -> String` vs a post-commit pool that
/// contains only `fn replace_with_caps(s: &str) -> String`.
///
/// The two spans are structurally near-identical (same signature shape, same
/// body, only the trailing `ures` characters differ in the name).  With the
/// current `TextDiff::ratio` heuristic at threshold 0.6 the candidate fires
/// and returns `Some("replace_with_caps")` — a false positive.
///
/// **Why it fails on HEAD a6b7e5c:**
/// `TextDiff::from_chars` counts character-level similarity across the full
/// span bytes.  `replace_with_captures` and `replace_with_caps` differ in
/// only four characters out of a long identical suffix, pushing the ratio
/// well above 0.6.  A correctly-hardened heuristic must reject this case
/// (the function was deleted, not renamed; the new symbol is an independent
/// addition).
#[test]
fn hp3_3b_replacement_deletion_no_false_rename_candidate() {
    let before = b"pub fn replace_with_captures(s: &str) -> String { s.to_string() }";
    let after_candidates = vec![(
        "replace_with_caps".to_string(),
        b"pub fn replace_with_caps(s: &str) -> String { s.to_string() }".to_vec(),
    )];
    let result = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(
        result, None,
        "replace_with_captures → replace_with_caps must NOT be treated as a \
         rename (the original was deleted; the similar name is an independent \
         addition), but rename_candidate returned {:?}",
        result
    );
}

// ── HP3-3 (Cluster C): field-drop rename false positive ──────────────────────

/// T0 field span: `all_verbatim_literal: bool`.
/// Post-commit candidate pool: `any_literal: bool` (the field was dropped,
/// not renamed; `any_literal` was already present in the struct at T0).
///
/// **Why it fails on HEAD a6b7e5c:**
/// `TextDiff::from_chars` on these two short spans produces a ratio above 0.6
/// because both share the `_literal: bool` suffix.  The heuristic treats this
/// as a rename and returns `Some("AstAnalysis::any_literal")`.  The correct
/// result is `None` — the field was deleted, and `any_literal` is a distinct,
/// pre-existing field whose qualified name is unrelated.
#[test]
fn hp3_3_field_drop_no_false_rename_candidate() {
    // Field spans as they appear in the AST byte slice used by
    // `rename_candidates_for` (just the field content, no surrounding struct).
    let before = b"all_verbatim_literal: bool";
    let after_candidates = vec![(
        "AstAnalysis::any_literal".to_string(),
        b"any_literal: bool".to_vec(),
    )];
    let result = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(
        result, None,
        "dropping `all_verbatim_literal` with `any_literal` remaining must NOT \
         be treated as a rename (they are distinct fields; the suffix `_literal: \
         bool` inflates similarity), but rename_candidate returned {:?}",
        result
    );
}

// ── HP3-4: rename true positive (preservation) ──────────────────────────────

/// `fn locations_mut(&mut self) -> …` becomes `fn captures_mut(&mut self) -> …`
/// in the same `impl AutomataCaptures` block.
///
/// The spans are near-identical (same return type, same body, only the
/// function name differs).  `TextDiff::ratio ≥ 0.6` must fire and return
/// `Some("captures_mut")`.
///
/// This test is GREEN on HEAD a6b7e5c and must remain GREEN through Task 4.
/// If Task 4's heuristic changes cause it to return `None`, that task must
/// document the regression and add a SPEC §11 entry for the rule change.
#[test]
fn hp3_4_rename_true_positive_same_impl_is_detected() {
    let before = b"fn locations_mut(&mut self) -> &mut Vec<Location> { &mut self.locations }";
    let after_candidates = vec![(
        "captures_mut".to_string(),
        b"fn captures_mut(&mut self) -> &mut Vec<Location> { &mut self.locations }".to_vec(),
    )];
    let result = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(
        result.as_deref(),
        Some("captures_mut"),
        "locations_mut → captures_mut in the same impl block must be detected \
         as a rename (same-container, near-identical span), but rename_candidate \
         returned {:?}",
        result
    );
}

// ── Additional leaf-name gate unit cases ─────────────────────────────────────
//
// These tests independently verify the two-part gate logic added in Task 4:
// (1) sibling-overlap FPs return None even when span ratio is above min_ratio;
// (2) same-context true-positive renames return Some.

/// When a function with a near-identical name exists in the post-commit pool
/// (one name is a pluralised/suffixed form of the other), it must NOT be
/// treated as a rename even though the span ratio is very high.
///
/// This is an independent unit case for the leaf-name upper-bound gate
/// (`MAX_NAME_SIMILARITY = 0.85`).  Companion to
/// `hp3_3b_replacement_deletion_no_false_rename_candidate`.
///
/// `fetch_datum` vs `fetch_data`: leaf-name similarity ≈ 0.857, span ratio
/// ≈ 0.976. The leaf-name similarity meets or exceeds the 0.85 upper bound
/// → the candidate must be rejected.
#[test]
fn near_identical_leaf_name_is_not_a_rename() {
    let before = b"fn fetch_data(client: &Client) -> Vec<Row> { client.query() }";
    let after_candidates = vec![(
        "fetch_datum".to_string(),
        b"fn fetch_datum(client: &Client) -> Vec<Row> { client.query() }".to_vec(),
    )];
    let result = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(
        result, None,
        "fetch_data → fetch_datum must NOT be treated as a rename \
         (near-identical name, leaf-name similarity ≥ 0.85), \
         but rename_candidate returned {:?}",
        result
    );
}

/// When a field with a loosely related name but same type exists as the only
/// post-commit candidate, it must NOT be treated as a rename.
///
/// `query_result_count` vs `total_count`: leaf-name similarity ≈ 0.483,
/// which falls below the lower bound (`min_ratio = 0.6`), so the candidate
/// is rejected.
#[test]
fn loosely_related_field_name_is_not_a_rename() {
    let before = b"query_result_count: u32";
    let after_candidates = vec![(
        "MyStruct::total_count".to_string(),
        b"total_count: u32".to_vec(),
    )];
    let result = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(
        result, None,
        "query_result_count → total_count must NOT be treated as a rename \
         (leaf-name similarity below min_ratio), but rename_candidate \
         returned {:?}",
        result
    );
}

/// A genuine same-context rename with clearly different but structurally
/// related names returns Some.
///
/// `fn build_query(…)` renamed to `fn build_filter(…)` — both share the
/// `build_` prefix.  Leaf-name similarity ≈ 0.696 ∈ [0.6, 0.85); span
/// ratio is high.  The two-part gate must pass and return `Some("build_filter")`.
#[test]
fn genuine_rename_shared_prefix_is_detected() {
    let before = b"fn build_query(params: &Params) -> String { format!(\"{:?}\", params) }";
    let after_candidates = vec![
        (
            "build_filter".to_string(),
            b"fn build_filter(params: &Params) -> String { format!(\"{:?}\", params) }".to_vec(),
        ),
        ("unrelated_fn".to_string(), b"fn unrelated_fn() {}".to_vec()),
    ];
    let result = rename_candidate(before, &after_candidates, 0.6);
    assert_eq!(
        result.as_deref(),
        Some("build_filter"),
        "build_query → build_filter must be detected as a rename \
         (same body, shared prefix, leaf-name similarity in valid range), \
         but rename_candidate returned {:?}",
        result
    );
}

// ── Typed pipeline (container threading + T₀-presence check) ─────────────────
//
// These tests exercise `rename_candidate_typed` which adds:
//   Gate 1 — container compatibility check
//   Gate 2 — T₀-presence exclusion (candidate must not have been a T₀ fact)
//   Gate 4 — version-suffix bypass for `_v<N>` evolution renames

/// `Struct::field_v1` → `Struct::field_v2`: same container, version suffix
/// evolution rename.  Leaf-name similarity ≈ 0.857 (above `MAX_NAME_SIMILARITY`
/// 0.85), but the version-suffix bypass in the typed pipeline waives the upper
/// bound when both names share the same base with different `_v<N>` suffixes.
/// Must return `Some("Struct::field_v2")`.
#[test]
fn typed_version_suffix_field_evolution_is_detected() {
    let origin = RenameOrigin::new("Struct::field_v1", b"field_v1: bool");
    let candidates = vec![RenameCandidate::new(
        "Struct::field_v2".to_string(),
        b"field_v2: bool".to_vec(),
    )];
    // `field_v2` is new at post-commit (NOT in T₀ names).
    let t0_names: HashSet<String> = ["Struct::field_v1".to_string()].into();
    let result = rename_candidate_typed(&origin, &candidates, &t0_names, 0.6);
    assert_eq!(
        result.as_deref(),
        Some("Struct::field_v2"),
        "field_v1 → field_v2 in same struct must be detected as a version-suffix \
         evolution rename (upper bound waived), but rename_candidate_typed returned {:?}",
        result
    );
}

/// `fn serialize` → `fn serialize_v2`: function evolution rename.
/// Top-level function (no container); version suffix `_v2` bypasses upper bound.
#[test]
fn typed_version_suffix_function_evolution_is_detected() {
    let origin = RenameOrigin::new(
        "serialize",
        b"pub fn serialize(data: &Data) -> Vec<u8> { data.to_bytes() }",
    );
    let candidates = vec![RenameCandidate::new(
        "serialize_v2".to_string(),
        b"pub fn serialize_v2(data: &Data) -> Vec<u8> { data.to_bytes() }".to_vec(),
    )];
    // `serialize_v2` is new at post-commit.
    let t0_names: HashSet<String> = ["serialize".to_string()].into();
    let result = rename_candidate_typed(&origin, &candidates, &t0_names, 0.6);
    assert_eq!(
        result.as_deref(),
        Some("serialize_v2"),
        "serialize → serialize_v2 must be detected as a version-suffix evolution \
         rename, but rename_candidate_typed returned {:?}",
        result
    );
}

/// Cross-container false positive: `Foo::name` deleted, `Bar::name` survives.
///
/// Both fields have leaf name `name` and identical type/body, which gives very
/// high span similarity.  Container check (Gate 1) must reject because the
/// origin's container is `"Foo"` and the candidate's container is `"Bar"`.
/// Returns `None`.
#[test]
fn typed_cross_container_field_is_rejected() {
    let origin = RenameOrigin::new("Foo::name", b"name: String");
    let candidates = vec![RenameCandidate::new(
        "Bar::name".to_string(),
        b"name: String".to_vec(),
    )];
    let t0_names: HashSet<String> = ["Foo::name".to_string()].into();
    let result = rename_candidate_typed(&origin, &candidates, &t0_names, 0.6);
    assert_eq!(
        result, None,
        "Foo::name → Bar::name must NOT be treated as a rename (different containers), \
         but rename_candidate_typed returned {:?}",
        result
    );
}

/// Cross-impl false positive for functions: both `impl Foo { fn bar }` and
/// `impl Quux { fn bar }` appear as bare top-level names `"bar"` (the extractor
/// does not track `impl` context in the qualified name).  However, the key
/// insight is that `"bar"` already existed as a T₀ fact — the T₀-presence
/// check (Gate 2) must reject it.
///
/// If the post-commit `"bar"` was NOT a T₀ fact, the two functions would both
/// appear as top-level (container = `None`) and container threading alone cannot
/// disambiguate them.  That case is out-of-scope for Phase 0b (the per-commit
/// index catches cross-file moves, and within-file the T₀-presence check handles
/// the same-container sibling case).
#[test]
fn typed_t0_present_candidate_is_rejected() {
    // `bar` exists at T₀ as an independent function; it must not be treated
    // as the rename target of another deleted T₀ function.
    let origin = RenameOrigin::new("do_work", b"fn do_work(x: u32) -> u32 { x * 2 }");
    let candidates = vec![RenameCandidate::new(
        "bar".to_string(),
        b"fn bar(x: u32) -> u32 { x * 2 }".to_vec(),
    )];
    // `bar` was already a T₀ fact.
    let t0_names: HashSet<String> = ["do_work".to_string(), "bar".to_string()].into();
    let result = rename_candidate_typed(&origin, &candidates, &t0_names, 0.6);
    assert_eq!(
        result, None,
        "A T₀-present candidate must NOT be treated as a rename target \
         (it is a surviving sibling, not a renamed version), \
         but rename_candidate_typed returned {:?}",
        result
    );
}

/// End-to-end typed rename: `Struct::value_count` → `Struct::result_count`.
/// Same container, candidate NOT in T₀, sufficient similarity.
#[test]
fn typed_genuine_same_container_rename_is_detected() {
    let origin = RenameOrigin::new("Struct::value_count", b"value_count: usize");
    let candidates = vec![
        RenameCandidate::new(
            "Struct::result_count".to_string(),
            b"result_count: usize".to_vec(),
        ),
        RenameCandidate::new(
            "Other::value_count".to_string(),
            b"value_count: usize".to_vec(),
        ),
    ];
    let t0_names: HashSet<String> = ["Struct::value_count".to_string()].into();
    let result = rename_candidate_typed(&origin, &candidates, &t0_names, 0.6);
    assert_eq!(
        result.as_deref(),
        Some("Struct::result_count"),
        "value_count → result_count in same struct must be detected; \
         Other::value_count is a T₀ fact and must be excluded by Gate 2, \
         but rename_candidate_typed returned {:?}",
        result
    );
}

// ── RenameCandidate::new_python tests ────────────────────────────────────────

#[test]
fn rename_candidate_new_python_strips_module_path() {
    let candidate = RenameCandidate::new_python(
        "src.a.Greeter.greet".into(),
        b"def greet(self): pass".to_vec(),
        Path::new("src/a.py"),
    );
    assert_eq!(candidate.container, Some("Greeter".to_string()));
    assert_eq!(candidate.leaf_name, "greet");
}

#[test]
fn rename_candidate_new_python_module_level() {
    let candidate = RenameCandidate::new_python(
        "src.a.foo".into(),
        b"def foo(): pass".to_vec(),
        Path::new("src/a.py"),
    );
    assert_eq!(candidate.container, None);
    assert_eq!(candidate.leaf_name, "foo");
}
