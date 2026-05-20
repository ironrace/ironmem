//! Post-commit fact matching helpers.
//!
//! Given a T₀ fact and the post-commit blob/AST for its source file, these
//! functions locate the corresponding post-commit entry (if any) or enumerate
//! same-kind candidates for rename detection.  They have no dependency on
//! [`super::Replay`], [`super::CommitState`], or `run_inner` — everything
//! they need comes in through their parameters.

use crate::ast::{spans::Span, RustAst};
use crate::diff::RenameCandidate;
use crate::facts::{doc_claim, field, function_signature, symbol_existence, test_assertion, Fact};
use anyhow::Result;
use std::path::Path;

/// Search the post-commit blob/AST for the same fact (by kind + qualified
/// key).  Returns `Ok(Some((post_span, post_content_hash)))` when found,
/// `Ok(None)` when the symbol is absent, or `Err` when the post-commit
/// blob cannot be parsed (e.g. invalid UTF-8 in a markdown file).
///
/// `test_assertion_ordinal` carries the zero-based position of a
/// `Fact::TestAssertion` among same-`(source_path, test_fn)` siblings at T₀,
/// computed by `replay::push_test_assertion_facts`. It is consumed only by
/// the `Fact::TestAssertion` arm to disambiguate which post-commit
/// assertion to compare against — without it, a test fn with N
/// assertions silently collapses to assertion #1 for every match
/// (pre-pass-4 bug).
///
/// - For non-`TestAssertion` facts the value is ignored.
/// - For `Fact::TestAssertion` it MUST be `Some(n)`; receiving `None`
///   indicates a constructed-without-`push_test_assertion_facts`
///   programming error and triggers a panic rather than a silent
///   misclassification.
///
/// `function_signature_disambiguator` carries the
/// `(cfg_attribute_set, impl_receiver_type, ordinal)` private replay-
/// time disambiguator for a `Fact::FunctionSignature`, computed by
/// `replay::push_function_signature_facts`. It is consumed only by the
/// `Fact::FunctionSignature` arm to pair T₀ → post when multiple
/// definitions share the same `qualified_name` (cfg-gated variants or
/// multi-impl `fn`s). Same fail-loud contract as the TestAssertion
/// ordinal: a `None` here for a `Fact::FunctionSignature` is a
/// programming error and panics.
///
/// **For every fact kind other than `Fact::FunctionSignature`,
/// callers must pass `None`** — those arms do not read the
/// disambiguator. Same applies to `test_assertion_ordinal` for every
/// kind other than `Fact::TestAssertion`. The two parameters are
/// per-kind contract pieces, not generic context.
pub(super) fn matching_post_fact(
    fact: &Fact,
    path: &Path,
    post_bytes: &[u8],
    post_ast: Option<&RustAst>,
    test_assertion_ordinal: Option<usize>,
    function_signature_disambiguator: Option<&super::FnDisambiguator>,
    commit_sha: &str,
) -> Result<Option<(Span, String)>> {
    match fact {
        Fact::FunctionSignature { qualified_name, .. } => Ok(post_ast.and_then(|ast| {
            let t0_disamb = function_signature_disambiguator.expect(
                "Fact::FunctionSignature must carry an FnDisambiguator; \
                 see replay::push_function_signature_facts. Routing through \
                 None would silently misclassify post-commit signatures.",
            );
            // Enumerate post-commit observations with matching
            // qualified_name AND matching disambiguator
            // (cfg_set, impl_receiver). Use .nth(ordinal) so genuine
            // duplicates under the same primary key are addressed in
            // tree-sitter walk order.
            function_signature::extract_observations(ast, path)
                .filter(|obs| match &obs.fact {
                    Fact::FunctionSignature {
                        qualified_name: q, ..
                    } => q == qualified_name,
                    _ => false,
                })
                .filter(|obs| {
                    let post_cfg: std::collections::BTreeSet<String> =
                        obs.cfg_attribute_set.iter().cloned().collect();
                    post_cfg == t0_disamb.cfg_set
                        && obs.impl_receiver_type == t0_disamb.impl_receiver
                })
                .nth(t0_disamb.ordinal)
                .map(|obs| match obs.fact {
                    Fact::FunctionSignature {
                        span, content_hash, ..
                    } => (span, content_hash),
                    _ => unreachable!("filter above guarantees FunctionSignature"),
                })
        })),
        Fact::Field { qualified_path, .. } => Ok(post_ast.and_then(|ast| {
            field::extract(ast, path).find_map(|f| match f {
                Fact::Field {
                    qualified_path: q,
                    span,
                    content_hash,
                    ..
                } if q == *qualified_path => Some((span, content_hash)),
                Fact::FunctionSignature { .. }
                | Fact::Field { .. }
                | Fact::PublicSymbol { .. }
                | Fact::DocClaim { .. }
                | Fact::TestAssertion { .. } => None,
            })
        })),
        Fact::PublicSymbol {
            qualified_name,
            content_hash: observed_hash,
            ..
        } => Ok(post_ast.and_then(|ast| {
            use symbol_existence::PublicSymbolForm;
            // Collect post-state PublicSymbol occurrences matching the
            // T₀ fact's qualified_name. Distinguishes Definition
            // (`pub fn`/`pub struct`/etc.) from BarePubUse (`pub use …
            // name;` or `pub use … Original as name;`). Pass-5 Cluster G
            // semantics: Definition is the primary match; BarePubUse
            // is a surface-continuity fallback that preserves Valid
            // when the form changed from direct definition to re-export.
            let occurrences: Vec<_> = symbol_existence::extract_occurrences(ast, path)
                .filter(|o| match &o.fact {
                    Fact::PublicSymbol {
                        qualified_name: q, ..
                    } => q == qualified_name,
                    _ => false,
                })
                .collect();

            // Prefer Definition: if one exists, return its (span, hash)
            // as today. Upstream `classify_against_commit`:
            //   - matching hash → Valid (unchanged).
            //   - differing hash → StaleSourceChanged.
            // This preserves pass-3's structural-source-changed behavior
            // for `pub fn X` body changes and similar.
            if let Some(def) = occurrences
                .iter()
                .find(|o| matches!(o.form, PublicSymbolForm::Definition))
            {
                if let Fact::PublicSymbol {
                    span, content_hash, ..
                } = &def.fact
                {
                    return Some((span.clone(), content_hash.clone()));
                }
            }

            // No Definition matched. Check for a BarePubUse occurrence
            // for the same exported name. SPEC §5 surface-continuity:
            // the public symbol `qualified_name` is still publicly
            // exported, just via a `pub use` re-export instead of a
            // direct definition. Return the bare-pub-use post span
            // (safe to slice from `post_bytes` upstream) paired with
            // the T₀ observed_hash so `structural = post_hash !=
            // observed_hash` evaluates `false` → Valid.
            //
            // Note: `extract_use_declaration` is gated by
            // `is_bare_pub`, so `pub(crate) use`, `pub(super) use`,
            // `pub(in …) use`, and plain `use` do NOT emit
            // PublicSymbolOccurrence::BarePubUse — they fall through
            // to the visibility-narrowing path below.
            if let Some(bpu) = occurrences
                .iter()
                .find(|o| matches!(o.form, PublicSymbolForm::BarePubUse { .. }))
            {
                if let Fact::PublicSymbol { span, .. } = &bpu.fact {
                    return Some((span.clone(), observed_hash.clone()));
                }
            }

            // Neither Definition nor BarePubUse found at the same
            // qualified_name. Check whether the item still exists at
            // narrowed visibility (pass-3 path): pub fn X → pub(crate)
            // fn X stays here.
            let simple_name = qualified_name
                .rsplit("::")
                .next()
                .expect("rsplit always yields at least one element");
            symbol_existence::find_item_by_name(ast, simple_name).and_then(|found| {
                use symbol_existence::VisibilityKind;
                match found.visibility {
                    VisibilityKind::BarePub => None,
                    VisibilityKind::Restricted | VisibilityKind::Private => {
                        Some((found.span, found.content_hash))
                    }
                }
            })
        })),
        Fact::DocClaim {
            qualified_name,
            mention_span,
            mention_hash,
            ..
        } => {
            // Do NOT anchor to the original byte offset: content inserted above
            // the mention shifts its position without changing its text.  Instead,
            // scan the post-commit markdown for all inline-code mentions whose
            // text equals `qualified_name`, then pick the best one via the
            // duplicate-mention tie-breaker.
            let candidates =
                doc_claim::find_mentions(post_bytes, path, commit_sha, qualified_name)?;
            let best = doc_claim::best_mention(&candidates, mention_span, mention_hash);
            Ok(best.map(|m| (m.span.clone(), m.mention_hash.clone())))
        }
        Fact::TestAssertion { test_fn, .. } => Ok(post_ast.and_then(|ast| {
            // Pair T₀ → post by `(test_fn, ordinal)`. `test_assertion::extract`
            // emits one fact per `assert!`/`assert_eq!`/`assert_ne!` invocation
            // in tree-sitter walk order, which matches the ordering used at T₀
            // (see `replay::push_test_assertion_facts`). Returning the
            // post fact at index `ordinal` makes a body-modified assertion at
            // the same position route to `StaleSourceChanged` via
            // `post_hash != observed_hash`, and an unchanged sibling stay
            // `Valid`. An out-of-range ordinal (deleted-tail assertion) yields
            // `None`, which upstream `classify_against_commit` routes via the
            // existing "symbol not found" path — `NeedsRevalidation` when
            // the test fn survives in `commit_index`, `StaleSourceDeleted`
            // otherwise. SPEC §5 rationale in
            // `benchmarks/provbench/spotcheck/2026-05-12-post-pass3-findings.md`.
            let ordinal = test_assertion_ordinal.expect(
                "Fact::TestAssertion must carry an ordinal; \
                 see replay::push_test_assertion_facts. Routing through \
                 None would silently misclassify post-commit assertions.",
            );
            test_assertion::extract(ast, path, &[])
                .filter_map(|f| match f {
                    Fact::TestAssertion {
                        test_fn: q,
                        span,
                        content_hash,
                        ..
                    } if q == *test_fn => Some((span, content_hash)),
                    Fact::FunctionSignature { .. }
                    | Fact::Field { .. }
                    | Fact::PublicSymbol { .. }
                    | Fact::DocClaim { .. }
                    | Fact::TestAssertion { .. } => None,
                })
                .nth(ordinal)
        })),
    }
}

/// Enumerate all same-kind facts in the post-commit blob for use as rename
/// candidates.  Returns `(qualified_name, span_bytes)` pairs.
pub(super) fn rename_candidates_for(
    fact: &Fact,
    path: &Path,
    post_bytes: &[u8],
    post_ast: Option<&RustAst>,
) -> Vec<(String, Vec<u8>)> {
    let Some(ast) = post_ast else {
        return Vec::new();
    };
    match fact {
        Fact::FunctionSignature { .. } => function_signature::extract(ast, path)
            .filter_map(|f| match f {
                Fact::FunctionSignature {
                    qualified_name,
                    span,
                    ..
                } => Some((qualified_name, post_bytes[span.byte_range].to_vec())),
                Fact::Field { .. }
                | Fact::PublicSymbol { .. }
                | Fact::DocClaim { .. }
                | Fact::TestAssertion { .. } => None,
            })
            .collect(),
        Fact::Field { .. } => field::extract(ast, path)
            .filter_map(|f| match f {
                Fact::Field {
                    qualified_path,
                    span,
                    ..
                } => Some((qualified_path, post_bytes[span.byte_range].to_vec())),
                Fact::FunctionSignature { .. }
                | Fact::PublicSymbol { .. }
                | Fact::DocClaim { .. }
                | Fact::TestAssertion { .. } => None,
            })
            .collect(),
        Fact::PublicSymbol { .. } => symbol_existence::extract(ast, path)
            .filter_map(|f| match f {
                Fact::PublicSymbol {
                    qualified_name,
                    span,
                    ..
                } => Some((qualified_name, post_bytes[span.byte_range].to_vec())),
                Fact::FunctionSignature { .. }
                | Fact::Field { .. }
                | Fact::DocClaim { .. }
                | Fact::TestAssertion { .. } => None,
            })
            .collect(),
        Fact::DocClaim { .. } => Vec::new(),
        Fact::TestAssertion { .. } => test_assertion::extract(ast, path, &[])
            .filter_map(|f| match f {
                Fact::TestAssertion { test_fn, span, .. } => {
                    Some((test_fn, post_bytes[span.byte_range].to_vec()))
                }
                Fact::FunctionSignature { .. }
                | Fact::Field { .. }
                | Fact::PublicSymbol { .. }
                | Fact::DocClaim { .. } => None,
            })
            .collect(),
    }
}

/// Typed variant of [`rename_candidates_for`].
///
/// Returns [`RenameCandidate`] values whose `container` and `leaf_name` are
/// pre-computed from the qualified name, ready for the typed filter pipeline
/// in [`crate::diff::rename_candidate_typed`].
pub(super) fn rename_candidates_for_typed(
    fact: &Fact,
    path: &Path,
    post_bytes: &[u8],
    post_ast: Option<&RustAst>,
) -> Vec<RenameCandidate> {
    rename_candidates_for(fact, path, post_bytes, post_ast)
        .into_iter()
        .map(|(qname, span)| RenameCandidate::new(qname, span))
        .collect()
}

/// Python equivalent of [`matching_post_fact`]. Five fact-kind arms.
///
/// SPEC §11 row 2026-05-19 (Plan A.2). Mirrors the Rust contract where
/// applicable but uses `PythonAst` and `facts::python::*` extractors.
/// PublicSymbol (Task 6), TestAssertion (Task 7), DocClaim (Task 8 stub)
/// arms land in subsequent tasks.
// Tasks 6-8 will wire callers from the Python replay path; suppress dead_code
// until the call site lands.
#[allow(dead_code)]
pub(super) fn matching_post_fact_python(
    fact: &Fact,
    path: &Path,
    _post_bytes: &[u8],
    post_ast: Option<&crate::ast::python::PythonAst>,
    test_assertion_ordinal: Option<usize>,
) -> Option<(Span, String)> {
    let ast = post_ast?;
    match fact {
        Fact::FunctionSignature { qualified_name, .. } => {
            crate::facts::python::function_signature::extract(ast, path).find_map(|f| match f {
                Fact::FunctionSignature {
                    qualified_name: q,
                    span,
                    content_hash,
                    ..
                } if q == *qualified_name => Some((span, content_hash)),
                _ => None,
            })
        }
        Fact::Field { qualified_path, .. } => crate::facts::python::field::extract(ast, path)
            .find_map(|f| match f {
                Fact::Field {
                    qualified_path: q,
                    span,
                    content_hash,
                    ..
                } if q == *qualified_path => Some((span, content_hash)),
                _ => None,
            }),
        Fact::PublicSymbol { qualified_name, .. } => {
            crate::facts::python::symbol_existence::extract(ast, path).find_map(|f| match f {
                Fact::PublicSymbol {
                    qualified_name: q,
                    span,
                    content_hash,
                    ..
                } if q == *qualified_name => {
                    // SPEC §11 row 2026-05-19: single-underscore-prefixed
                    // leaf is NOT a continuation of a public name.
                    let leaf = q.rsplit('.').next().unwrap_or(&q);
                    if leaf.starts_with('_') {
                        return None;
                    }
                    Some((span, content_hash))
                }
                _ => None,
            })
        }
        Fact::TestAssertion { test_fn, .. } => {
            let ordinal = test_assertion_ordinal.expect(
                "Fact::TestAssertion must carry an ordinal; \
                 see replay::push_test_assertion_facts. Routing through \
                 None would silently misclassify post-commit assertions.",
            );
            crate::facts::python::test_assertion::extract(ast, path)
                .filter_map(|f| match f {
                    Fact::TestAssertion {
                        test_fn: q,
                        span,
                        content_hash,
                        ..
                    } if q == *test_fn => Some((span, content_hash)),
                    _ => None,
                })
                .nth(ordinal)
        }
        Fact::DocClaim { .. } => {
            // SPEC §11 row 2026-05-19 (Plan A.2): Python DocClaim semantics
            // are out of scope. The Python facts::python::doc_claim extractor
            // is a stub today. DocClaim facts on Python paths route through
            // upstream as NeedsRevalidation. Revisit in v1.4+.
            None
        }
    }
}

#[cfg(test)]
mod python_match_tests {
    use super::*;
    use crate::ast::python::PythonAst;
    use std::path::Path;

    #[test]
    fn python_function_signature_matches_by_qualified_name() {
        let src = b"def foo():\n    return 1\n";
        let ast = PythonAst::parse(src).unwrap();
        let path = Path::new("src/a.py");
        let fact = Fact::FunctionSignature {
            qualified_name: "src.a.foo".into(),
            source_path: path.to_path_buf(),
            span: crate::ast::spans::Span {
                byte_range: 0..src.len(),
                line_start: 1,
                line_end: 1,
            },
            content_hash: "old-hash".into(),
        };
        let result = matching_post_fact_python(&fact, path, src, Some(&ast), None);
        assert!(
            result.is_some(),
            "expected match for src.a.foo at src/a.py; got None"
        );
    }

    #[test]
    fn python_function_signature_misses_on_different_name() {
        let src = b"def bar():\n    return 1\n";
        let ast = PythonAst::parse(src).unwrap();
        let path = Path::new("src/a.py");
        let fact = Fact::FunctionSignature {
            qualified_name: "src.a.foo".into(),
            source_path: path.to_path_buf(),
            span: crate::ast::spans::Span {
                byte_range: 0..src.len(),
                line_start: 1,
                line_end: 1,
            },
            content_hash: "old-hash".into(),
        };
        let result = matching_post_fact_python(&fact, path, src, Some(&ast), None);
        assert!(
            result.is_none(),
            "expected None for foo when only bar exists"
        );
    }

    #[test]
    fn python_public_symbol_matches_module_level_def() {
        let src = b"def foo():\n    return 1\n";
        let ast = PythonAst::parse(src).unwrap();
        let path = Path::new("src/a.py");
        let fact = Fact::PublicSymbol {
            qualified_name: "src.a.foo".into(),
            source_path: path.to_path_buf(),
            span: crate::ast::spans::Span {
                byte_range: 0..src.len(),
                line_start: 1,
                line_end: 1,
            },
            content_hash: "old-hash".into(),
        };
        let result = matching_post_fact_python(&fact, path, src, Some(&ast), None);
        assert!(result.is_some(), "expected PublicSymbol match for foo");
    }

    #[test]
    fn python_public_symbol_underscore_renames_to_none() {
        // Post-commit source only has `_foo`; T0 fact has `qualified_name: "src.a.foo"`.
        // The extractor emits "src.a._foo" — no name match — so result is None.
        // The underscore leaf filter is defense-in-depth for any residual match.
        let src = b"def _foo():\n    return 1\n";
        let ast = PythonAst::parse(src).unwrap();
        let path = Path::new("src/a.py");
        let fact = Fact::PublicSymbol {
            qualified_name: "src.a.foo".into(),
            source_path: path.to_path_buf(),
            span: crate::ast::spans::Span {
                byte_range: 0..src.len(),
                line_start: 1,
                line_end: 1,
            },
            content_hash: "old-hash".into(),
        };
        let result = matching_post_fact_python(&fact, path, src, Some(&ast), None);
        assert!(
            result.is_none(),
            "expected None when public foo became private _foo"
        );
    }

    #[test]
    fn python_field_matches_by_qualified_path() {
        let src = b"class Greeter:\n    name = 'world'\n";
        let ast = PythonAst::parse(src).unwrap();
        let path = Path::new("src/a.py");
        let fact = Fact::Field {
            qualified_path: "src.a.Greeter.name".into(),
            source_path: path.to_path_buf(),
            type_text: String::new(),
            span: crate::ast::spans::Span {
                byte_range: 17..32,
                line_start: 2,
                line_end: 2,
            },
            content_hash: "old-hash".into(),
        };
        let result = matching_post_fact_python(&fact, path, src, Some(&ast), None);
        assert!(result.is_some(), "expected match for src.a.Greeter.name");
    }

    #[test]
    fn python_test_assertion_pairs_by_test_fn_and_ordinal() {
        let src = b"def test_thing():\n    assert 1 == 1\n    assert 2 == 2\n";
        let ast = PythonAst::parse(src).unwrap();
        let path = Path::new("tests/test_thing.py");
        let fact = Fact::TestAssertion {
            test_fn: "test_thing".into(),
            source_path: path.to_path_buf(),
            span: crate::ast::spans::Span {
                byte_range: 0..src.len(),
                line_start: 1,
                line_end: 3,
            },
            content_hash: "old-hash".into(),
            asserted_symbol: None,
        };
        // ordinal=1 (second assertion).
        let result = matching_post_fact_python(&fact, path, src, Some(&ast), Some(1));
        assert!(
            result.is_some(),
            "expected match for second assertion in test_thing; got None"
        );

        // Out-of-range ordinal=5.
        let result_oob = matching_post_fact_python(&fact, path, src, Some(&ast), Some(5));
        assert!(
            result_oob.is_none(),
            "expected None for out-of-range ordinal=5"
        );
    }

    #[test]
    fn python_doc_claim_returns_none_per_design() {
        let src = b"# README\nSome text\n";
        let ast = PythonAst::parse(src).unwrap();
        let path = Path::new("README.md");
        let fact = Fact::DocClaim {
            qualified_name: "src.a.foo".into(),
            doc_path: path.to_path_buf(),
            mention_span: crate::ast::spans::Span {
                byte_range: 0..10,
                line_start: 1,
                line_end: 1,
            },
            mention_hash: "h".into(),
            defining_span: crate::ast::spans::Span {
                byte_range: 0..10,
                line_start: 1,
                line_end: 1,
            },
            defining_hash: "h2".into(),
        };
        let result = matching_post_fact_python(&fact, path, src, Some(&ast), None);
        assert!(
            result.is_none(),
            "DocClaim Python arm returns None per design"
        );
    }
}
