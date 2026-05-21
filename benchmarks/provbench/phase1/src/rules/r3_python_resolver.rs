//! R3 v1.4 Python AST resolver — SPEC §5.2 amendment.
//!
//! Returns true iff `leaf` matches the name of any declaration node in
//! `post_blob` parsed as Python. See
//! `docs/superpowers/specs/2026-05-21-r3-retuning-v1.4-design.md` §5.3
//! for the counted/excluded declaration kinds.

use tree_sitter::{Parser, Query, QueryCursor, StreamingIterator};

/// True iff `leaf` matches the identifier of a Python declaration in
/// `post_blob`. Conservative on parse failure / empty / non-UTF8:
/// returns true (prefer false-Valid over false-Stale, per v1.4
/// V-retention goal).
///
/// Counted declaration kinds (design §5.3):
///   - `function_definition` (incl. methods)
///   - `class_definition`
///   - `decorated_definition` wrapping a function or class
///   - module-scope assignment LHS (single identifier only)
///   - class-body-scope assignment LHS (single identifier only)
///
/// Intentionally NOT counted:
///   - local variables inside function bodies
///   - function parameters
///   - identifier uses (e.g., `print(foo)`)
///   - tuple-unpacking assignment LHS (e.g., `a, b = 1, 2`)
pub fn resolves_in_python(post_blob: &[u8], leaf: &str) -> bool {
    if post_blob.is_empty() {
        return true;
    }
    let Ok(source) = std::str::from_utf8(post_blob) else {
        return true;
    };

    let mut parser = Parser::new();
    let language: tree_sitter::Language = tree_sitter_python::LANGUAGE.into();
    if parser.set_language(&language).is_err() {
        return true;
    }

    let Some(tree) = parser.parse(source, None) else {
        return true;
    };

    // Conservative fallback on parse error: tree-sitter is error-tolerant
    // and will still return a tree, but with ERROR nodes when input is
    // malformed. Per design §5.3, we prefer false-Valid over false-Stale,
    // so any ERROR in the root subtree triggers the conservative path.
    if tree.root_node().has_error() {
        return true;
    }

    // Query: capture the `name` identifier of each counted declaration
    // kind. Module-/class-scope assignments are matched via their
    // enclosing block to exclude function-local and nested-block
    // assignments.
    let query_src = r#"
        (function_definition name: (identifier) @name)
        (class_definition name: (identifier) @name)
        (decorated_definition (function_definition name: (identifier) @name))
        (decorated_definition (class_definition name: (identifier) @name))
        (module (expression_statement (assignment left: (identifier) @name)))
        (class_definition body: (block (expression_statement (assignment left: (identifier) @name))))
    "#;

    let Ok(query) = Query::new(&language, query_src) else {
        // Query compile failure would be a code bug; stay conservative.
        return true;
    };

    let mut cursor = QueryCursor::new();
    let source_bytes = source.as_bytes();
    let mut matches = cursor.matches(&query, tree.root_node(), source_bytes);

    while let Some(m) = matches.next() {
        for capture in m.captures {
            if let Ok(name) = capture.node.utf8_text(source_bytes) {
                if name == leaf {
                    return true;
                }
            }
        }
    }
    false
}
