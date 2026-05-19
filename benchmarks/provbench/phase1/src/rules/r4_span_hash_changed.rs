//! R4 span_lines_present — SPEC §5.3 (Phase 1 line-presence variant).
//!
//! SPEC §10 pilot tuning: the labeler's `content_hash_at_observation` is
//! computed over a sub-line byte range that phase1 cannot reproduce from
//! `line_span` alone (the byte range comes from tree-sitter span
//! extraction at labeling time, but only the line numbers leak into the
//! emitted fact JSON). Phase 1 instead checks whether the **T0 source
//! lines that bound the fact** still appear verbatim somewhere in the
//! post-commit file:
//!
//! 1. Extract `t0_span` = T0 `lines[start..=end]` (the fact's bound
//!    lines exactly).
//! 2. If the leaf-or-length guard passes AND `t0_span ⊂ post_blob`
//!    → **Valid** (the fact's bound source is byte-stable; line
//!    numbers may have shifted but the code didn't).
//! 3. Else if the leaf-or-length guard passes AND `t0_span ⊄ post_blob`
//!    → **Stale** (the lines that bound the fact have meaningfully changed).
//! 4. Else (guard fails — probe is below floor or missing the leaf
//!    symbol) → **NeedsRevalidation** (phase1 has no defensible probe;
//!    aligns phase1's output taxonomy with the labeler's three classes).
//!    SPEC §11 row 2026-05-18 (v1.2c, rule_set_version v1.3).
//!
//! Rationale: this trades the labeler-internal `byte_range`+hash check
//! for a byte-window presence check that phase1 can compute from
//! line_span alone. Valid rows with intra-file line shifts (the bulk
//! of canary GT=Valid rows) are caught by the substring match. The
//! leaf-symbol guard prevents trivial single-token lines from masking
//! genuine staleness.
//!
//! ## v1.2 Field carve-out
//!
//! The `MIN_PROBE_NONWS_LEN = 8` floor is skipped for `kind = "Field"`;
//! `probe_has_leaf` alone is the sanity gate. Empirical justification:
//! 132 of 162 R4 false-Stale rows on the serde §9.4 held-out canary
//! were `Field` facts like `'    c: C,\n'` (nonws_len = 4) whose
//! `t0_span` was byte-identical in `post_blob` — the comparator
//! was correct, the length floor rejected it. Other kinds are
//! unchanged: `TestAssertion` keeps its `MIN_PROBE_NONWS_LEN_ASSERTION`
//! floor, and all other kinds keep the v1.1 `probe_has_leaf &&
//! nonws_len >= 8` gate.

use super::{Decision, RowCtx, Rule};

/// Minimum non-whitespace probe length below which we refuse to use the
/// T0 line slice as a uniqueness probe. Lines this short are
/// dominated by syntax noise (`}`, `{`, `;`, `*/`) and produce too many
/// false-positive matches.
const MIN_PROBE_NONWS_LEN: usize = 8;

/// Stricter length guard used for `TestAssertion` facts: the leaf
/// identifier is the *containing test fn* name, not anything on the
/// asserted line, so we lean on length alone to avoid matching
/// trivial assertions like `assert!(x);`.
const MIN_PROBE_NONWS_LEN_ASSERTION: usize = 20;

pub struct R4SpanHashChanged;

impl Rule for R4SpanHashChanged {
    fn rule_id(&self) -> &'static str {
        "R4"
    }
    fn spec_ref(&self) -> &'static str {
        "SPEC §5.3"
    }
    fn classify(&self, ctx: &RowCtx<'_>) -> Option<(Decision, String)> {
        let (Some(post), Some(t0)) = (ctx.post_blob, ctx.t0_blob) else {
            return None;
        };
        let line_start = ctx.fact.line_span[0];
        let line_end = ctx.fact.line_span[1];
        let t0_span = extract_lines(t0, [line_start, line_end]);

        // Out-of-bounds span → no signal; fall through.
        if t0_span.is_empty() {
            return None;
        }

        // Leaf-symbol guard (kind-conditional):
        //   - For symbol-bearing kinds (FunctionSignature/Field/
        //     PublicSymbol), the leaf identifier IS the line — drop
        //     the guard if it's already in the probe.
        //   - For TestAssertion the leaf is the *containing test fn*
        //     name; the fact's line is the assertion body itself,
        //     which doesn't mention the test fn — so the leaf guard
        //     would over-reject. Use a stricter length guard instead.
        let leaf = leaf_symbol(&ctx.fact.symbol_path);
        let probe_has_leaf =
            !leaf.is_empty() && t0_span.windows(leaf.len()).any(|w| w == leaf.as_bytes());

        // Min-probe-length guard: too-short probes (after stripping
        // whitespace) are ambiguous noise.
        let nonws_len = t0_span.iter().filter(|b| !b.is_ascii_whitespace()).count();

        let guard_passed = match ctx.fact.kind.as_str() {
            "TestAssertion" => nonws_len >= MIN_PROBE_NONWS_LEN_ASSERTION,
            // v1.2: drop length floor for `Field` — the leaf-symbol
            // presence check is the sanity floor. The v1.1 floor of 8
            // rejected exact-byte matches on short single-letter
            // fields (`c: C,`, `t: T,`) that drove 132 of 162 R4
            // false-Stale rows on the serde §9.4 held-out canary.
            // See docs/superpowers/specs/2026-05-15-provbench-v1.2a-r4-guard-design.md.
            "Field" => probe_has_leaf,
            _ => probe_has_leaf && nonws_len >= MIN_PROBE_NONWS_LEN,
        };

        if guard_passed && contains_subslice(post, &t0_span) {
            return Some((
                Decision::Valid,
                serde_json::json!({
                    "rule": "R4",
                    "reason": "t0_span_found_in_post",
                    "guard_below_floor": !guard_passed,
                })
                .to_string(),
            ));
        }

        // SPEC §11 row 2026-05-18 (v1.2c, rule_set_version v1.3):
        // Split the ambiguous path. When the leaf-or-length guard FAILS
        // (we have no defensible probe to test), route to NR — phase1
        // cannot make a confident structural claim. When the guard
        // PASSES but the probe is absent from post_blob, the source
        // lines that bound this fact have meaningfully changed → Stale.
        if !guard_passed {
            return Some((
                Decision::NeedsRevalidation,
                serde_json::json!({
                    "rule": "R4",
                    "reason": "guard_below_floor",
                    "guard_below_floor": true,
                    "kind": ctx.fact.kind,
                    "nonws_len": nonws_len,
                    "probe_has_leaf": probe_has_leaf,
                })
                .to_string(),
            ));
        }

        Some((
            Decision::Stale,
            serde_json::json!({
                "rule": "R4",
                "reason": "stale_source_changed",
                "guard_below_floor": !guard_passed,
            })
            .to_string(),
        ))
    }
}

fn leaf_symbol(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

fn extract_lines(src: &[u8], span: [u64; 2]) -> Vec<u8> {
    let mut out = Vec::new();
    let (start, end) = (span[0] as usize, span[1] as usize);
    if start == 0 || end < start {
        return out;
    }
    for (i, line) in src.split(|&b| b == b'\n').enumerate() {
        let lineno = i + 1;
        if lineno >= start && lineno <= end {
            out.extend_from_slice(line);
            out.push(b'\n');
        }
        if lineno > end {
            break;
        }
    }
    out
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}
