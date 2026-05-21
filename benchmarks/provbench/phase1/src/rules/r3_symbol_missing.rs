//! R3 symbol_missing — SPEC §5.2.
//! Symbol no longer resolves at the original path AND R7 didn't fire
//! -> Stale (stale_source_deleted).

use super::{Decision, RowCtx, Rule};

pub struct R3SymbolMissing;

impl Rule for R3SymbolMissing {
    fn rule_id(&self) -> &'static str {
        "R3"
    }
    fn spec_ref(&self) -> &'static str {
        "SPEC §5.2"
    }
    fn classify(&self, ctx: &RowCtx<'_>) -> Option<(Decision, String)> {
        if !matches!(
            ctx.fact.kind.as_str(),
            "FunctionSignature" | "Field" | "PublicSymbol"
        ) {
            return None;
        }
        let (Some(post), _t0) = (ctx.post_blob, ctx.t0_blob) else {
            return None;
        };
        // SPEC §10 pilot tuning: search for the **leaf** symbol name
        // (last `::`-separated component) rather than the qualified path.
        // Rust source never literally contains `Type::field`; the field is
        // declared inside its parent struct on a separate line.
        let qualified = ctx.fact.symbol_path.as_str();
        let leaf = leaf_symbol(qualified);
        if leaf.is_empty() {
            return None; // defensive: empty/unparseable symbol_path
        }
        // v1.4: language-aware dispatch. Python facts route through
        // the AST resolver; Rust path is byte-identical to v1.3.
        if is_python_fact(&ctx.fact.fact_id) {
            let py_leaf = leaf_python_symbol(qualified);
            if py_leaf.is_empty() {
                return None; // defensive
            }
            if super::r3_python_resolver::resolves_in_python(post, py_leaf) {
                return None;
            }
            return Some((
                Decision::Stale,
                serde_json::json!({
                    "rule": "R3",
                    "reason": "stale_source_deleted",
                    "lang": "python",
                    "symbol": qualified,
                    "leaf": py_leaf,
                })
                .to_string(),
            ));
        }
        let needle = leaf.as_bytes();
        let haystack = post;
        // Naive substring search — symbol no longer literally appears.
        let resolves = haystack.windows(needle.len()).any(|w| w == needle);
        if !resolves {
            return Some((
                Decision::Stale,
                serde_json::json!({
                    "rule": "R3",
                    "reason": "stale_source_deleted",
                    "symbol": qualified,
                    "leaf": leaf,
                })
                .to_string(),
            ));
        }
        None
    }
}

/// Extract the leaf segment of a `::`-qualified symbol path (e.g.
/// `Type::method` → `method`, `module::Type::method` → `method`).
/// Returns the input unchanged when no `::` is present.
fn leaf_symbol(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// Extract the leaf segment of a Python `.`-qualified symbol path
/// (e.g. `module.Class.method` → `method`, `module.func` → `func`).
/// Returns the input unchanged when no `.` is present.
fn leaf_python_symbol(qualified: &str) -> &str {
    qualified.rsplit('.').next().unwrap_or(qualified)
}

/// Detect whether a fact identifier refers to a Python source line.
/// Fact IDs are shaped `<kind>::<dotted_symbol>::<path>::<line>`.
fn is_python_fact(fact_id: &str) -> bool {
    // Path component ends in `.py` immediately before the trailing
    // `::<line>`. Tolerate paths containing `::` themselves by looking
    // at the substring before the last `::`.
    let Some(last_sep) = fact_id.rfind("::") else {
        return false;
    };
    let before_line = &fact_id[..last_sep];
    before_line.ends_with(".py")
}

#[cfg(test)]
mod tests {
    use super::is_python_fact;
    use super::leaf_python_symbol;

    #[test]
    fn python_leaf_extracts_last_dot_component() {
        assert_eq!(
            leaf_python_symbol("tests.test_structures.TestLookupDict.test_get"),
            "test_get"
        );
    }

    #[test]
    fn python_leaf_handles_module_level_function() {
        assert_eq!(
            leaf_python_symbol("requests.utils.iter_slices"),
            "iter_slices"
        );
    }

    #[test]
    fn python_leaf_passthrough_when_no_dot() {
        assert_eq!(leaf_python_symbol("foo"), "foo");
    }

    #[test]
    fn python_fact_detected() {
        assert!(is_python_fact(
            "FunctionSignature::flask.app.Flask.add_url_rule::src/flask/app.py::1234"
        ));
    }

    #[test]
    fn rust_fact_not_detected() {
        assert!(!is_python_fact(
            "FunctionSignature::ripgrep::cli::run::src/cli.rs::42"
        ));
    }

    #[test]
    fn empty_fact_id_returns_false() {
        assert!(!is_python_fact(""));
    }

    #[test]
    fn fact_without_separator_returns_false() {
        assert!(!is_python_fact("nopath"));
    }
}
