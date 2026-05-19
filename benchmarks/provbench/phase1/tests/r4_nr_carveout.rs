//! SPEC §11 row 2026-05-18 (v1.2c forward path c): R4 returns
//! `Decision::NeedsRevalidation` when `post_blob` is Some AND the
//! leaf-or-length guard fails. The "post_blob contains t0_span"
//! → Valid path is unchanged.

use provbench_phase1::facts::FactBody;
use provbench_phase1::rules::r4_span_hash_changed::R4SpanHashChanged;
use provbench_phase1::rules::{Decision, RowCtx, Rule};

fn fact_for(symbol_path: &str, line_span: [u64; 2], kind: &str) -> FactBody {
    FactBody {
        fact_id: "test".into(),
        kind: kind.into(),
        body: "".into(),
        source_path: "src/lib.rs".into(),
        symbol_path: symbol_path.into(),
        line_span,
        content_hash_at_observation: "deadbeef".into(),
        labeler_git_sha: "".into(),
    }
}

#[test]
fn r4_returns_nr_when_guard_below_floor_and_post_blob_present() {
    // T0: `}` (single brace line — well below MIN_PROBE_NONWS_LEN=8 and
    // leaf="frobnicate" not present, so guard fails).
    let t0 = b"}\n".to_vec();
    let post = b"// rewritten\n".to_vec();
    let fact = fact_for("foo::bar::frobnicate", [1, 1], "Function");

    let ctx = RowCtx {
        fact: &fact,
        commit_sha: "abc",
        diff: None,
        post_blob: Some(&post),
        t0_blob: Some(&t0),
        post_tree: None,
        commit_files: &[],
    };

    let (decision, evidence) = R4SpanHashChanged.classify(&ctx).unwrap();
    assert_eq!(
        decision,
        Decision::NeedsRevalidation,
        "R4 must route ambiguous (guard-below-floor + post_blob present) rows to NR, not Stale"
    );
    let parsed: serde_json::Value = serde_json::from_str(&evidence).unwrap();
    assert_eq!(parsed["rule"], "R4");
    assert_eq!(parsed["guard_below_floor"], true);
}

#[test]
fn r4_still_returns_valid_when_t0_span_subset_of_post() {
    // T0: a probe line that is clearly present in post.
    let t0 = b"    pub fn frobnicate(&self) -> i32 {\n".to_vec();
    let post = b"// header\n    pub fn frobnicate(&self) -> i32 {\n        42\n    }\n".to_vec();
    let fact = fact_for("foo::bar::frobnicate", [1, 1], "Function");

    let ctx = RowCtx {
        fact: &fact,
        commit_sha: "abc",
        diff: None,
        post_blob: Some(&post),
        t0_blob: Some(&t0),
        post_tree: None,
        commit_files: &[],
    };

    let (decision, _evidence) = R4SpanHashChanged.classify(&ctx).unwrap();
    assert_eq!(
        decision,
        Decision::Valid,
        "the t0_span ⊂ post_blob → Valid path must NOT change in v1.3"
    );
}

#[test]
fn r4_still_returns_stale_when_guard_passes_and_probe_absent() {
    // Probe is long enough AND contains leaf, but not in post.
    let t0 = b"    pub fn frobnicate(&self) -> i32 { 42 }\n".to_vec();
    let post = b"// totally rewritten file with no frobnicate at all\n".to_vec();
    let fact = fact_for("foo::bar::frobnicate", [1, 1], "Function");

    let ctx = RowCtx {
        fact: &fact,
        commit_sha: "abc",
        diff: None,
        post_blob: Some(&post),
        t0_blob: Some(&t0),
        post_tree: None,
        commit_files: &[],
    };

    let (decision, _evidence) = R4SpanHashChanged.classify(&ctx).unwrap();
    assert_eq!(
        decision,
        Decision::Stale,
        "guard-passing absent-probe rows remain Stale — only guard-failing rows reroute to NR"
    );
}
