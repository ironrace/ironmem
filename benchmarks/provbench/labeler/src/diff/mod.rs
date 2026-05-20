//! Per SPEC §5 rule 3: whitespace-only or comment-only diffs do not invalidate
//! a fact even when content hashes differ. Implementation tokenizes both
//! sides with tree-sitter, drops trivia, and compares the residual.

use crate::lang::Language;
use crate::replay::commit_index::split_python_qualified_name;
use std::collections::HashSet;
use tree_sitter::{Node, Parser, Tree};

/// Context-lines argument to `git diff -U<N>`. SPEC §6.1 requires
/// "full file context for affected hunks" — we use a value far larger
/// than any conceivable source file so the unified diff effectively
/// includes the entire touched file. Increase only if a real file
/// exceeds this length (extremely unlikely).
const FULL_FILE_CONTEXT_LINES: &str = "-U999999";

/// Reject any string that isn't exactly 40 lowercase hex characters.
///
/// Defense-in-depth before a commit SHA is passed as an argv element
/// to `git`. Without this guard, a corpus row containing
/// `--upload-pack=…`, `HEAD:path`, `refs/…`, or any other git option
/// or revision spelling could be smuggled into the subprocess argv via
/// the `parent`/`commit` parameters. Restricting to 40 lowercase hex
/// chars precisely matches a Git object ID and rejects every other form.
fn validate_sha(s: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        s.len() == 40 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "commit SHA must be 40 lowercase hex chars, got: {s:?}"
    );
    Ok(())
}

/// Compute the unified diff between two commits with full file context
/// (`-U999999`) restricted to files actually touched in the commit.
///
/// Used by the `emit-diffs` subcommand (SPEC §6.1) to produce per-commit
/// diff artifacts for the Phase 0c baseline runner. Invokes the system
/// `git` binary via `std::process::Command` with an explicit arg-vector
/// (no shell interpolation), against the repo at `repo_path`.
pub fn full_file_context_diff(
    repo_path: &std::path::Path,
    parent: &str,
    commit: &str,
) -> anyhow::Result<String> {
    use anyhow::Context;
    validate_sha(parent)?;
    validate_sha(commit)?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["diff", FULL_FILE_CONTEXT_LINES, parent, commit])
        .output()
        .context("git diff invocation failed")?;
    anyhow::ensure!(
        output.status.success(),
        "git diff returned non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

/// Return the first-parent SHA of `commit`, or `None` if `commit` is a
/// root commit (i.e. `git rev-parse <commit>^` fails).
///
/// Used by the `emit-diffs` subcommand to discriminate the `no_parent`
/// exclusion case from the diffable case.
pub fn parent_sha(repo_path: &std::path::Path, commit: &str) -> anyhow::Result<Option<String>> {
    validate_sha(commit)?;
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .args(["rev-parse", &format!("{commit}^")])
        .output()?;
    if output.status.success() {
        Ok(Some(String::from_utf8(output.stdout)?.trim().to_string()))
    } else {
        Ok(None)
    }
}

/// Per SPEC §5 rule 3: whitespace-only or comment-only diffs do not invalidate
/// a fact even when content hashes differ.
///
/// Tokenizes both `before` and `after` with the tree-sitter grammar for
/// `language`, drops comment nodes and pure-whitespace leaf tokens, and
/// compares the residual token byte sequences for byte-equality.
///
/// Comment node kinds by language:
/// - Rust: `line_comment`, `block_comment`
/// - Python: `comment` (Python has no block-comment grammar; triple-quoted
///   docstrings parse as `string` expressions and are significant tokens
///   that are NOT dropped).
pub fn is_whitespace_or_comment_only(before: &[u8], after: &[u8], language: Language) -> bool {
    let parse = |s: &[u8]| -> Option<Tree> {
        let mut p = Parser::new();
        let lang_grammar = match language {
            Language::Rust => tree_sitter_rust::LANGUAGE.into(),
            Language::Python => tree_sitter_python::LANGUAGE.into(),
        };
        p.set_language(&lang_grammar).ok()?;
        p.parse(s, None)
    };
    let Some(b_tree) = parse(before) else {
        return false;
    };
    let Some(a_tree) = parse(after) else {
        return false;
    };
    let mut b_toks: Vec<&[u8]> = Vec::new();
    let mut a_toks: Vec<&[u8]> = Vec::new();
    collect_significant_tokens(b_tree.root_node(), before, language, &mut b_toks);
    collect_significant_tokens(a_tree.root_node(), after, language, &mut a_toks);
    b_toks == a_toks
}

fn collect_significant_tokens<'a>(
    node: Node<'_>,
    src: &'a [u8],
    language: Language,
    out: &mut Vec<&'a [u8]>,
) {
    let kind = node.kind();
    let is_comment = match language {
        Language::Rust => kind == "line_comment" || kind == "block_comment",
        Language::Python => kind == "comment",
    };
    if is_comment {
        return;
    }
    if node.child_count() == 0 {
        let s = &src[node.byte_range()];
        if !s.iter().all(|b| b.is_ascii_whitespace()) {
            out.push(s);
        }
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_significant_tokens(child, src, language, out);
    }
}

/// Upper-bound on leaf-name similarity for rename detection.
///
/// When two candidates share a near-identical name (e.g. `replace_with_captures`
/// vs `replace_with_caps`, similarity ≈ 0.89), the high span similarity is
/// driven almost entirely by the near-identical name rather than structural
/// evidence.  Such pairs represent independent sibling symbols, not renames.
/// Candidates whose leaf-name similarity meets or exceeds this bound are
/// excluded from rename detection.
const MAX_NAME_SIMILARITY: f32 = 0.85;

/// Per SPEC §5 rule 2: when a symbol no longer resolves, search post-commit
/// candidates for one whose Myers-diff similarity (via
/// `similar::TextDiff::ratio()`) is ≥ `min_ratio` over symbol-bearing lines.
///
/// Two-part gate (both must pass):
/// 1. **Span similarity** ≥ `min_ratio` — confirms the body/signature is
///    structurally close enough to warrant rename consideration.
/// 2. **Leaf-name similarity** in [`min_ratio`, `MAX_NAME_SIMILARITY`) — the
///    candidate's leaf name must resemble the original (lower bound: some
///    naming relationship exists) but must not be nearly identical (upper
///    bound: prevents sibling symbols whose high span-level ratio is driven
///    entirely by a nearly-unchanged name from being treated as renames).
///
/// Returns the best (highest span-ratio) candidate name above both thresholds,
/// or `None`.
///
/// # Deprecation note
///
/// Production code now uses [`rename_candidate_typed`] exclusively (wired into
/// `classify_against_commit` in `replay/mod.rs`).  This function is retained
/// as `#[doc(hidden)]` for the existing diff-level unit tests and the HP3-4
/// integration preservation test in `tests/replay_hardening.rs`.  Migrate those
/// tests to the typed API in a follow-up commit, then remove this function.
#[doc(hidden)]
pub fn rename_candidate(
    before_span: &[u8],
    after_candidates: &[(String, Vec<u8>)],
    min_ratio: f32,
) -> Option<String> {
    let before = String::from_utf8_lossy(before_span);
    let before_leaf = extract_leaf_name_from_span(&before);
    let mut best: Option<(String, f32)> = None;
    for (name, span) in after_candidates {
        let after = String::from_utf8_lossy(span);
        let span_ratio = similar::TextDiff::from_chars(before.as_ref(), after.as_ref()).ratio();
        if span_ratio < min_ratio {
            continue;
        }
        // Gate 2: leaf-name similarity must be in [min_ratio, MAX_NAME_SIMILARITY).
        // — Lower bound: the candidate's name must have some resemblance to the
        //   original (rules out completely unrelated symbols that happen to share
        //   a common body pattern).
        // — Upper bound: the candidate's name must differ enough to represent a
        //   genuine rename rather than a coincidental sibling with a nearly-
        //   identical name.
        let candidate_leaf = leaf_name_from_qualified(name);
        let name_ratio = similar::TextDiff::from_chars(before_leaf, candidate_leaf).ratio();
        if name_ratio < min_ratio || name_ratio >= MAX_NAME_SIMILARITY {
            continue;
        }
        match &best {
            None => best = Some((name.clone(), span_ratio)),
            Some((_, r)) if span_ratio > *r => best = Some((name.clone(), span_ratio)),
            _ => {}
        }
    }
    best.map(|(n, _)| n)
}

// ── Typed rename-candidate pipeline ──────────────────────────────────────────

/// A post-commit fact that is a candidate rename target for a deleted T₀ fact.
///
/// Carries enough structural context for the filter pipeline to apply
/// container-threading and T₀-presence checks before falling back to the
/// similarity gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameCandidate {
    /// Fully-qualified name as stored by the extractor
    /// (e.g. `"AstAnalysis::any_literal"`, `"captures_mut"`).
    pub qualified_name: String,
    /// Last `::` segment of `qualified_name`.
    pub leaf_name: String,
    /// Everything before the last `::` segment, or `None` for top-level symbols
    /// with no container (bare function names, top-level public symbols).
    pub container: Option<String>,
    /// Raw span bytes for similarity comparison.
    pub span: Vec<u8>,
}

impl RenameCandidate {
    /// Build from a qualified name and span bytes.  Container is derived by
    /// splitting on `::`: `"Struct::field"` → container `"Struct"`;
    /// `"E::Variant::field"` → container `"E::Variant"`; `"bare"` → `None`.
    pub fn new(qualified_name: String, span: Vec<u8>) -> Self {
        let (container, leaf_name) = split_container_leaf(&qualified_name);
        Self {
            leaf_name: leaf_name.to_string(),
            container: container.map(str::to_string),
            span,
            qualified_name,
        }
    }

    /// SPEC §11 row 2026-05-19 (Plan A.2): Python-aware constructor.
    /// Strips the module-path prefix derived from `path` (e.g.
    /// `src.a.Greeter.greet` at `src/a.py` → container `Some("Greeter")`,
    /// leaf `"greet"`). Splits remaining dotted segments at the last `.`.
    /// Module-level symbols with no class prefix produce `container: None`.
    ///
    /// Do NOT use `RenameCandidate::new` for Python — that splits on
    /// `::` and dotted Python names produce wrong containers.
    ///
    /// Delegates to the canonical `split_python_qualified_name` from
    /// `commit_index` so the logic is byte-stable with the index population
    /// and `python_fact_kind_container_leaf` in `replay/mod.rs`.
    pub fn new_python(qualified_name: String, span: Vec<u8>, path: &std::path::Path) -> Self {
        let (container_str, leaf_name) = split_python_qualified_name(&qualified_name, path);
        let container = if container_str.is_empty() {
            None
        } else {
            Some(container_str)
        };
        Self {
            qualified_name,
            span,
            container,
            leaf_name,
        }
    }
}

/// The T₀ fact that is being matched against rename candidates.
///
/// Only the fields needed by the filter pipeline are included.
pub struct RenameOrigin<'a> {
    /// Fully-qualified name of the deleted T₀ fact.
    pub qualified_name: &'a str,
    /// Last `::` segment of `qualified_name`.
    pub leaf_name: &'a str,
    /// Container prefix, or `None` for top-level symbols.
    pub container: Option<&'a str>,
    /// Raw span bytes.
    pub span: &'a [u8],
}

impl<'a> RenameOrigin<'a> {
    /// Build from a qualified name and span bytes.
    pub fn new(qualified_name: &'a str, span: &'a [u8]) -> Self {
        let (container, leaf_name) = split_container_leaf(qualified_name);
        Self {
            leaf_name,
            container,
            span,
            qualified_name,
        }
    }
}

/// Typed rename-candidate filter pipeline.
///
/// Filter steps (all must pass):
/// 1. **Container compatibility**: the candidate's container must match the
///    origin's container.  Two facts are container-compatible when both have
///    the same `Some(container)` string, or both are `None` (top-level).
///    This rejects cross-struct field false positives and cross-impl fn
///    false positives.
/// 2. **T₀ presence exclusion**: if the candidate's qualified name already
///    appears in `t0_qualified_names`, it was present at T₀ as an
///    independent fact — it cannot be the rename target of another T₀ fact.
///    This is the definitive fix for the `hp3_3` field-drop case.
/// 3. **Span similarity** ≥ `min_ratio` — same as the original gate.
/// 4. **Leaf-name similarity** gate:
///    - Lower bound: similarity ≥ `min_ratio` (naming evidence required).
///    - Upper bound: similarity < `MAX_NAME_SIMILARITY` (prevents near-identical
///      siblings like `replace_with_captures`/`replace_with_caps` from matching).
///    - **Version-suffix bypass**: if both the origin and candidate leaf names
///      share the same base name and differ only in a trailing `_v<N>` version
///      suffix (e.g. `field_v1` → `field_v2`), the upper bound is waived.
///      Version-suffix evolution renames are legitimate and the T₀-presence
///      check (Gate 2) already provides the structural guard for same-container
///      false positives.
///
/// Returns the best (highest span-ratio) matching candidate's qualified name,
/// or `None`.
pub fn rename_candidate_typed(
    origin: &RenameOrigin<'_>,
    candidates: &[RenameCandidate],
    t0_qualified_names: &HashSet<String>,
    min_ratio: f32,
) -> Option<String> {
    let before = String::from_utf8_lossy(origin.span);
    let mut best: Option<(String, f32)> = None;

    for candidate in candidates {
        // Gate 1: container must match.
        if candidate.container.as_deref() != origin.container {
            continue;
        }

        // Gate 2: candidate must not already have been a T₀ fact.
        // If `any_literal` was present at T₀ it is NOT a rename of
        // `all_verbatim_literal`; it is a distinct, surviving field.
        if t0_qualified_names.contains(&candidate.qualified_name) {
            continue;
        }

        // Gate 3: span similarity ≥ min_ratio.
        let after = String::from_utf8_lossy(&candidate.span);
        let span_ratio = similar::TextDiff::from_chars(before.as_ref(), after.as_ref()).ratio();
        if span_ratio < min_ratio {
            continue;
        }

        // Gate 4: leaf-name similarity gate.
        // Lower bound: naming evidence must exist.
        // Upper bound: skip near-identical-name siblings — unless the names
        //   differ only in a version suffix (`_v1` → `_v2`), in which case
        //   the upper bound is waived (version-suffix evolution is a valid rename).
        let name_ratio =
            similar::TextDiff::from_chars(origin.leaf_name, &candidate.leaf_name).ratio();
        if name_ratio < min_ratio {
            continue;
        }
        let is_version_suffix_evolution =
            version_suffix_bases_match(origin.leaf_name, &candidate.leaf_name);
        if name_ratio >= MAX_NAME_SIMILARITY && !is_version_suffix_evolution {
            continue;
        }

        match &best {
            None => best = Some((candidate.qualified_name.clone(), span_ratio)),
            Some((_, r)) if span_ratio > *r => {
                best = Some((candidate.qualified_name.clone(), span_ratio));
            }
            _ => {}
        }
    }
    best.map(|(n, _)| n)
}

/// Return `true` when both names form a version-suffix evolution pair.
///
/// Two cases are recognized:
/// 1. Both names have a `_v<N>` suffix and share the same base:
///    `"field_v1"` / `"field_v2"` (base = `"field"`, versions differ).
/// 2. The candidate has a `_v<N>` suffix and the origin is the un-versioned
///    base: `"serialize"` / `"serialize_v2"` (first versioning of the symbol).
///    The reverse (`"serialize_v2"` / `"serialize"`) is also accepted.
///
/// Examples:
/// - `"field_v1"`, `"field_v2"` → `true`  (same base, different versions)
/// - `"serialize"`, `"serialize_v2"` → `true`  (un-versioned → first version)
/// - `"serialize_v2"`, `"serialize_v3"` → `true`
/// - `"fetch_data"`, `"fetch_datum"` → `false`  (no version suffix on either)
/// - `"field_v1"`, `"field_v1"` → `false`  (identity, not rename)
fn version_suffix_bases_match(a: &str, b: &str) -> bool {
    fn strip_version_suffix(name: &str) -> Option<(&str, u64)> {
        // Match trailing `_v<digits>` where `<digits>` is non-empty.
        let underscore_v = name.rfind("_v")?;
        let suffix = &name[underscore_v + 2..];
        if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let version: u64 = suffix.parse().ok()?;
        Some((&name[..underscore_v], version))
    }
    // Both have `_v<N>` suffixes and share the same base with different versions.
    if let (Some((base_a, ver_a)), Some((base_b, ver_b))) =
        (strip_version_suffix(a), strip_version_suffix(b))
    {
        return base_a == base_b && ver_a != ver_b;
    }
    // One has a `_v<N>` suffix; the other is the un-versioned base
    // (e.g. `"serialize"` → `"serialize_v2"`).
    if let Some((base_b, _)) = strip_version_suffix(b) {
        if base_b == a {
            return true;
        }
    }
    if let Some((base_a, _)) = strip_version_suffix(a) {
        if base_a == b {
            return true;
        }
    }
    false
}

/// Split a qualified name into `(Option<container>, leaf)`.
///
/// `"AstAnalysis::any_literal"` → `(Some("AstAnalysis"), "any_literal")`
/// `"E::Variant::field"` → `(Some("E::Variant"), "field")`
/// `"captures_mut"` → `(None, "captures_mut")`
fn split_container_leaf(qualified: &str) -> (Option<&str>, &str) {
    match qualified.rfind("::") {
        Some(pos) => (Some(&qualified[..pos]), &qualified[pos + 2..]),
        None => (None, qualified),
    }
}

/// Extract the last `::` segment of a qualified name.
///
/// `"AstAnalysis::any_literal"` → `"any_literal"`
/// `"captures_mut"` → `"captures_mut"`
fn leaf_name_from_qualified(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

/// Best-effort extraction of the primary identifier (leaf name) from a raw
/// span byte slice (as stored by the labeler).
///
/// Handles the two most common forms:
/// - Function declaration: `[pub] fn <name>(` → extracts `<name>`.
/// - Field / other: leading identifier up to the first non-identifier byte.
///
/// Falls back to the full span string when no identifier boundary is found.
fn extract_leaf_name_from_span(span: &str) -> &str {
    // Try `fn <name>` pattern (function signatures).
    if let Some(after_fn) = span.find("fn ") {
        let rest = &span[after_fn + 3..];
        let end = rest
            .find(|c: char| !c.is_alphanumeric() && c != '_')
            .unwrap_or(rest.len());
        return &rest[..end];
    }
    // Fall back: leading identifier (field names, symbol names).
    // Skip leading `pub ` or `pub(…) ` if present.
    let trimmed = span.trim_start();
    let trimmed = trimmed
        .strip_prefix("pub(")
        .and_then(|s| s.find(')').map(|i| s[i + 1..].trim_start()))
        .or_else(|| trimmed.strip_prefix("pub "))
        .unwrap_or(trimmed);
    let end = trimmed
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(trimmed.len());
    &trimmed[..end]
}

#[cfg(test)]
mod sha_validation_tests {
    use super::validate_sha;

    #[test]
    fn accepts_canonical_40_lowercase_hex() {
        assert!(validate_sha(&"a".repeat(40)).is_ok());
        assert!(validate_sha("0123456789abcdef0123456789abcdef01234567").is_ok());
        assert!(validate_sha("af6b6c543b224d348a8876f0c06245d9ea7929c5").is_ok());
    }

    #[test]
    fn rejects_git_option_smuggling() {
        let err = validate_sha("--upload-pack=/x").unwrap_err();
        assert!(err.to_string().contains("40 lowercase hex"));
    }

    #[test]
    fn rejects_colon_path_form() {
        assert!(validate_sha("HEAD:path").is_err());
        assert!(validate_sha("refs/heads/main").is_err());
    }

    #[test]
    fn rejects_short_sha_and_uppercase() {
        assert!(validate_sha(&"a".repeat(39)).is_err());
        assert!(validate_sha(&"a".repeat(41)).is_err());
        // Uppercase hex is rejected even though git itself would accept
        // it — we want a single canonical form so two corpora never
        // disagree on equality of the same commit.
        assert!(validate_sha(&"A".repeat(40)).is_err());
    }

    #[test]
    fn rejects_empty_and_non_hex() {
        assert!(validate_sha("").is_err());
        assert!(validate_sha(&"z".repeat(40)).is_err());
        assert!(validate_sha(&"g".repeat(40)).is_err());
    }
}
