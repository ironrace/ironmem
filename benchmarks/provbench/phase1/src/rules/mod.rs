//! Structural rule chain (SPEC §5 step 4 first-match-wins).
//!
//! Execution order (RuleChain::classify_first_match):
//!   R0 -> R1 -> R2 -> R5 -> R6 -> R7 -> R3 -> R4 -> R8 -> R9
//! Numeric IDs are stable trace labels — not execution sequence.

use serde::{Deserialize, Serialize};

pub mod r0_diff_excluded;
pub mod r1_source_file_missing;
pub mod r2_blob_identical;
pub mod r3_symbol_missing;
pub mod r3_python_resolver;
pub mod r4_span_hash_changed;
pub mod r5_whitespace_or_comment_only;
pub mod r6_doc_claim;
pub mod r7_rename_candidate;
pub mod r8_ambiguous;
pub mod r9_fallback;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Valid,
    Stale,
    NeedsRevalidation,
}

impl Decision {
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Valid => "valid",
            Decision::Stale => "stale",
            Decision::NeedsRevalidation => "needs_revalidation",
        }
    }
}

/// Per-row context the rules consume. Built by the runner once per (commit, source_path).
pub struct RowCtx<'a> {
    pub fact: &'a crate::facts::FactBody,
    pub commit_sha: &'a str,
    /// Diff artifact for this commit, or None if absent.
    pub diff: Option<&'a crate::diffs::CommitDiff>,
    /// Post-commit blob for fact.source_path, or None if file missing.
    pub post_blob: Option<&'a [u8]>,
    /// T0 blob for fact.source_path (cached).
    pub t0_blob: Option<&'a [u8]>,
    /// Pre-parsed Rust file at the post-commit revision, if applicable.
    pub post_tree: Option<&'a crate::parse::ParsedFile>,
    /// Full tree listing at the post-commit revision (for rename search).
    pub commit_files: &'a [String],
}

pub trait Rule {
    fn rule_id(&self) -> &'static str;
    fn spec_ref(&self) -> &'static str;
    /// Returns `Some(Decision)` if this rule fires, with a JSON-encoded
    /// evidence blob for `rule_traces.jsonl`. Returns `None` to fall through.
    fn classify(&self, ctx: &RowCtx<'_>) -> Option<(Decision, String)>;
}

pub struct RuleChain {
    rules: Vec<Box<dyn Rule>>,
}

impl Default for RuleChain {
    fn default() -> Self {
        Self {
            rules: vec![
                Box::new(r0_diff_excluded::R0DiffExcluded),
                // R7 runs ahead of R1: both target `post_blob.is_none()` rows
                // (file absent at the commit). R7 gets first chance to claim
                // the row as a rename — leaf-symbol/basename similarity ≥0.6
                // against any same-extension path in the commit tree — and
                // falls through if no candidate matches, in which case R1
                // marks the row `stale_source_deleted`. Before this swap R1
                // pre-empted R7 unconditionally, making R7 dead code.
                Box::new(r7_rename_candidate::R7RenameCandidate),
                Box::new(r1_source_file_missing::R1SourceFileMissing),
                Box::new(r2_blob_identical::R2BlobIdentical),
                Box::new(r5_whitespace_or_comment_only::R5WhitespaceOrCommentOnly),
                Box::new(r6_doc_claim::R6DocClaim),
                Box::new(r3_symbol_missing::R3SymbolMissing),
                Box::new(r4_span_hash_changed::R4SpanHashChanged),
                Box::new(r8_ambiguous::R8Ambiguous),
                Box::new(r9_fallback::R9Fallback),
            ],
        }
    }
}

impl RuleChain {
    pub fn classify_first_match(
        &self,
        ctx: &RowCtx<'_>,
    ) -> (Decision, &'static str, &'static str, String) {
        for rule in &self.rules {
            if let Some((d, evidence)) = rule.classify(ctx) {
                return (d, rule.rule_id(), rule.spec_ref(), evidence);
            }
        }
        // R9 fallback always fires, so this is unreachable; defend anyway.
        (Decision::NeedsRevalidation, "R9", "SPEC §5.3", "{}".into())
    }
}
