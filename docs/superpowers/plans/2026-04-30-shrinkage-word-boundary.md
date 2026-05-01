# Word-boundary matching in `shrinkage_rerank` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace `String::contains` substring matching in `shrinkage_rerank` and its `idf_filter` helper with a precompiled word-boundary regex (with light suffix tolerance for common English inflections), gated behind `IRONMEM_SHRINKAGE_WORD_BOUNDARY` (default ON). Eliminates the false-positive scoring inflation where predicate keyword `"suggest"` substring-matches drawer text `"suggestions"` and `"current"` substring-matches `"currently"`.

**Architecture:** One new env tunable + two new private helpers (`compile_token_matcher` returns a `(?i)\b{kw}(?:s|es|ed|ing|ion|ions)?\b` regex; `token_hit` runs `Regex::is_match`). `shrinkage_rerank` and `idf_filter` each thread the tunable through a binary branch (boundary path vs legacy substring path) so the IDF filter and the scorer use the same matcher and their counts agree with rewards. Quoted phrases keep substring (intentional — user quotes are literal-string intent).

**Tech Stack:** Rust 2021. The `regex` crate (already a dep at `crates/ironmem/Cargo.toml:40`). No new deps.

**Spec:** `docs/superpowers/specs/2026-04-30-shrinkage-word-boundary-design.md` (commit `cd357ff`).

**Acceptance:**
1. ≥ +5pp R@5 on LongMemEval `single-session-preference` (n=30) with `IRONMEM_SHRINKAGE_WORD_BOUNDARY=1` (default) vs `=0`.
2. No other category regresses by more than −1pp R@5.
3. ≤ +5ms median search latency at the 50-drawer haystack scale.

---

## File map

| Path | Action | Purpose |
|---|---|---|
| `crates/ironmem/src/search/tunables.rs` | modify | Add `shrinkage_word_boundary_enabled()` reader |
| `crates/ironmem/src/search/rerank.rs` | modify | Add `compile_token_matcher` + `token_hit`; thread tunable through `shrinkage_rerank` (kw + name boosts) and `idf_filter` |
| `crates/ironmem/tests/shrinkage_word_boundary_test.rs` | create | 8 integration tests covering both modes |

---

## Task 1: Add `shrinkage_word_boundary_enabled()` tunable

**Files:**
- Modify: `crates/ironmem/src/search/tunables.rs`

- [ ] **Step 1: Read the existing tunables module to find the placement convention**

Run: `head -40 crates/ironmem/src/search/tunables.rs`

Expected: confirms `OnceLock`, `env_bool`, `env_usize` helpers and the per-section comment style (`// ── E5: preference enrichment ──`). Place the new tunable in its own section at the bottom of the file, matching how `pref_enrich_enabled` is structured.

- [ ] **Step 2: Append the new tunable**

Append to `crates/ironmem/src/search/tunables.rs`:

```rust
// ── shrinkage matcher mode ──────────────────────────────────────────────────

/// `IRONMEM_SHRINKAGE_WORD_BOUNDARY=0` reverts the shrinkage rerank's
/// keyword/name matcher to legacy substring behavior. Default ON.
///
/// The legacy path (`String::contains`) causes false-positive boosts: a
/// predicate keyword like "suggest" substring-matches drawer text
/// "suggestions"; "current" matches "currently". Word-boundary matching
/// with a small set of English suffix tolerances (s|es|ed|ing|ion|ions)
/// fixes the substring confusion without losing inflected-form recall.
///
/// Not OnceLock-cached: the integration tests need to flip this per-test.
pub fn shrinkage_word_boundary_enabled() -> bool {
    !matches!(
        std::env::var("IRONMEM_SHRINKAGE_WORD_BOUNDARY").as_deref(),
        Ok("0") | Ok("false")
    )
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p ironmem`
Expected: clean build. The function is unused at this point — the compiler may warn `function is never used`. That's expected; Task 3 wires it in.

If you see the unused-function warning and the project is configured with `-D warnings`, add `#[allow(dead_code)]` above the function temporarily, then remove it in Task 3 when the function gets its first caller. Verify with:

```bash
grep -n 'shrinkage_word_boundary_enabled' crates/ironmem/src/search/tunables.rs
```

- [ ] **Step 4: Commit**

```bash
git add crates/ironmem/src/search/tunables.rs
git commit -m "feat(ironmem): tunable shrinkage_word_boundary_enabled() (default on)"
```

---

## Task 2: Add `compile_token_matcher` + `token_hit` helpers with unit tests

**Files:**
- Modify: `crates/ironmem/src/search/rerank.rs`

- [ ] **Step 1: Write the failing unit tests first**

Append inside the existing `#[cfg(test)] mod tests { ... }` block at the bottom of `crates/ironmem/src/search/rerank.rs` (currently ends at line 373):

```rust
    #[test]
    fn token_matcher_exact_form_matches() {
        let m = compile_token_matcher("suggest");
        assert!(m.is_match("can you suggest a name?"));
    }

    #[test]
    fn token_matcher_inflected_forms_match() {
        let m = compile_token_matcher("suggest");
        for body in ["i suggested it", "she is suggesting", "any suggestions?", "one suggestion stands"] {
            assert!(m.is_match(body), "expected to match in {body:?}");
        }
    }

    #[test]
    fn token_matcher_does_not_match_unrelated_substring() {
        // "current" must NOT match "currently" — adverb -ly is not in the
        // suffix list. This is the photography-failure failure pattern.
        let m = compile_token_matcher("current");
        assert!(!m.is_match("we are currently shipping"), "currently must not match current");
    }

    #[test]
    fn token_matcher_does_not_match_prefix_extension() {
        // Front-edge boundary: the prefix `pre` makes this not a word-boundary match.
        let m = compile_token_matcher("suggest");
        assert!(!m.is_match("we presuggest carefully"));
    }

    #[test]
    fn token_matcher_escapes_metacharacters() {
        // Tokens with regex metacharacters must compile and match literally.
        let m = compile_token_matcher("c++");
        assert!(m.is_match("i write c++ daily"));
    }

    #[test]
    fn token_matcher_is_case_insensitive() {
        // Even though callers lowercase upstream, the (?i) flag belt-and-suspenders.
        let m = compile_token_matcher("photography");
        assert!(m.is_match("Photography setup notes"));
    }

    #[test]
    fn token_hit_wraps_is_match() {
        let m = compile_token_matcher("setup");
        assert!(token_hit("a clean setup of tools", &m));
        assert!(!token_hit("a clean setup_thing", &m));
    }
```

- [ ] **Step 2: Run the new tests to verify they fail**

Run: `cargo test -p ironmem --lib search::rerank::tests::token_matcher`
Expected: compile errors — `compile_token_matcher` and `token_hit` are not defined. (Or, if the engineer runs them after a partial implementation, the test names appear and fail.)

- [ ] **Step 3: Implement the two helpers**

In `crates/ironmem/src/search/rerank.rs`, near the top of the file just after the `// --- Regex patterns ---` block (around line 45, after the `QUOTED_RE` static), add:

```rust
// --- Word-boundary token matcher ---------------------------------------------

/// Compile a word-boundary matcher for a single token, with light suffix
/// tolerance for common English inflections. Reuse the returned regex
/// across all candidate documents for one query — compile cost is ~µs.
///
/// Pattern: `(?i)\b{escape(token)}(?:s|es|ed|ing|ion|ions)?\b`
///
/// - `(?i)` — case-insensitive (belt-and-suspenders; callers lowercase).
/// - `\b` — Unicode word boundary (regex crate default).
/// - `regex::escape` neutralizes regex metacharacters in the token.
/// - The optional suffix group covers verb→noun and tense inflections
///   common in English. `-ly` (adverbial) is intentionally excluded so
///   "current" does NOT match "currently".
fn compile_token_matcher(token: &str) -> Regex {
    let escaped = regex::escape(token);
    Regex::new(&format!(
        r"(?i)\b{escaped}(?:s|es|ed|ing|ion|ions)?\b"
    ))
    .expect("token regex must compile after escape")
}

/// Boundary-aware version of `doc.contains(token)`. Thin wrapper over
/// `Regex::is_match` so callers (the scorer and the IDF filter) share a
/// single hit-test seam.
fn token_hit(doc_lower: &str, matcher: &Regex) -> bool {
    matcher.is_match(doc_lower)
}
```

- [ ] **Step 4: Run tests; expect them to pass**

Run: `cargo test -p ironmem --lib search::rerank::tests::token_matcher`
Expected: 6 tests in the `token_matcher_*` family + `token_hit_wraps_is_match` all pass.

Run the full rerank test module to confirm nothing else broke:
Run: `cargo test -p ironmem --lib search::rerank::tests`
Expected: all green.

- [ ] **Step 5: Lint**

Run: `cargo clippy -p ironmem -- -D warnings`
Expected: no warnings. The two helpers are still unused outside the test module (Task 3 wires them in). If clippy complains about dead code, add `#[allow(dead_code)]` above each helper temporarily — Task 3 removes it.

- [ ] **Step 6: Commit**

```bash
git add crates/ironmem/src/search/rerank.rs
git commit -m "feat(rerank): compile_token_matcher + token_hit helpers"
```

---

## Task 3: Wire helpers into `shrinkage_rerank`'s `kw_boost` path

**Files:**
- Modify: `crates/ironmem/src/search/rerank.rs`
- Create: `crates/ironmem/tests/shrinkage_word_boundary_test.rs`

- [ ] **Step 1: Create the integration test scaffold and write the failing tests**

Create `crates/ironmem/tests/shrinkage_word_boundary_test.rs`:

```rust
//! Integration tests for the word-boundary matcher in `shrinkage_rerank`.
//!
//! Cases cover both branches of the `IRONMEM_SHRINKAGE_WORD_BOUNDARY`
//! tunable so the bench's A/B comparison is locked in by tests.

use std::sync::Mutex;

use ironmem::db::{Drawer, ScoredDrawer};
use ironmem::search::rerank::{extract_signals, shrinkage_rerank};

/// Serializes tests that mutate `IRONMEM_SHRINKAGE_WORD_BOUNDARY` because
/// the tunable reads the env var on every call but other tests in the
/// binary may read it concurrently.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn make(content: &str, score: f32) -> ScoredDrawer {
    ScoredDrawer {
        drawer: Drawer {
            id: "x".into(),
            content: content.into(),
            wing: "w".into(),
            room: "r".into(),
            source_file: "".into(),
            added_by: "".into(),
            filed_at: "".into(),
            date: "".into(),
        },
        score,
    }
}

#[test]
fn boundary_match_exact_form_lifts_correct_drawer() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");

    // Two drawers with the SAME initial RRF score. Only one mentions
    // "photography" exactly. Boundary mode lifts the relevant drawer.
    let mut cs = vec![
        make("photography accessories for my Sony camera", 0.50),
        make("a photographer once said something else entirely", 0.50),
    ];
    let signals = extract_signals("recommend photography gear");
    shrinkage_rerank(&mut cs, &signals);

    cs.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    assert!(
        cs[0].drawer.content.contains("photography accessories"),
        "exact 'photography' match should rank above 'photographer'"
    );

    std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
}

#[test]
fn boundary_match_inflected_forms_still_lift() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");

    // Verify the suffix-tolerance branch: "suggest" should boost a drawer
    // with "suggested" or "suggesting" (verb-form inflections).
    let mut cs = vec![
        make("she suggested the gitzo tripod last spring", 0.50),
        make("a totally unrelated baking recipe with flour", 0.55),
    ];
    let signals = extract_signals("can you suggest a tripod");
    shrinkage_rerank(&mut cs, &signals);

    cs.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    assert!(
        cs[0].drawer.content.contains("suggested"),
        "inflected-form match should rank the relevant drawer first"
    );

    std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
}

#[test]
fn boundary_does_not_inflate_unrelated_substring() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");

    // The photography-failure pattern: query has "current"; an unrelated
    // drawer says "currently". Under substring matching, "current" hits
    // "currently" and the unrelated drawer scores higher than the gold.
    // Under boundary matching, no hit — relative order preserved.
    let gold = make("Sony A7R IV camera flash and tripod options", 0.50);
    let unrelated = make("we are currently working out at the gym", 0.50);

    let mut cs = vec![unrelated.clone(), gold.clone()];
    let signals = extract_signals("complement my current photography setup");
    shrinkage_rerank(&mut cs, &signals);

    let unrelated_score_after = cs
        .iter()
        .find(|c| c.drawer.content.starts_with("we are currently"))
        .map(|c| c.score)
        .unwrap();
    let gold_score_after = cs
        .iter()
        .find(|c| c.drawer.content.starts_with("Sony A7R IV"))
        .map(|c| c.score)
        .unwrap();

    // 'photography' matches 'photography' word-boundary in gold via the
    // suffix-tolerance pattern (gold has 'photography'). Unrelated has
    // 'currently' but no 'current' word-boundary hit. Gold should not
    // be below unrelated under boundary mode.
    assert!(
        gold_score_after >= unrelated_score_after,
        "gold ({gold_score_after}) must not score below unrelated ({unrelated_score_after}) under boundary mode"
    );

    std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
}

#[test]
fn legacy_substring_still_inflates_when_disabled() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "0");

    // Same setup as above. Under legacy substring, "current" SHOULD hit
    // "currently" and inflate the unrelated drawer above gold. This proves
    // the tunable wires both paths — bench can A/B cleanly.
    let mut cs = vec![
        make("we are currently working out at the gym", 0.50),
        make("Sony A7R IV camera and tripod options", 0.50),
    ];
    let signals = extract_signals("complement my current photography setup");
    shrinkage_rerank(&mut cs, &signals);

    let unrelated_score = cs
        .iter()
        .find(|c| c.drawer.content.starts_with("we are currently"))
        .map(|c| c.score)
        .unwrap();
    let gold_score = cs
        .iter()
        .find(|c| c.drawer.content.starts_with("Sony A7R IV"))
        .map(|c| c.score)
        .unwrap();

    assert!(
        unrelated_score > gold_score,
        "legacy substring path must inflate 'currently' as a 'current' hit ({unrelated_score} vs {gold_score})"
    );

    std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
}
```

You'll need to verify `extract_signals` and `shrinkage_rerank` are publicly exported. Check `crates/ironmem/src/search/mod.rs` — `rerank` is `pub mod rerank;`, and inside `rerank.rs` both functions are `pub fn`. The test uses `ironmem::search::rerank::{extract_signals, shrinkage_rerank}` which is the canonical path.

If `Drawer` and `ScoredDrawer` are not re-exported from `ironmem::db`, fall back to `ironmem::db::drawers::{Drawer, ScoredDrawer}` (check `crates/ironmem/src/db/mod.rs` — earlier work in this branch added `Drawer` to the `pub use drawers::...` line).

- [ ] **Step 2: Run the integration tests; expect failures**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test`
Expected: `boundary_does_not_inflate_unrelated_substring` fails because `shrinkage_rerank` still uses `String::contains` and "current" still substring-matches "currently". The other three tests may pass coincidentally (the keyword "photography" matches both forms identically in substring mode) — but the photography-failure test is the load-bearing assertion.

- [ ] **Step 3: Modify `shrinkage_rerank` to thread the tunable through `kw_boost`**

In `crates/ironmem/src/search/rerank.rs`, locate `shrinkage_rerank` (currently at line 226). The current loop body computes `kw_boost` as:

```rust
let kw_boost = if effective_kws.is_empty() {
    0.0
} else {
    let hits = effective_kws
        .iter()
        .filter(|kw| doc.contains(kw.as_str()))
        .count();
    hits as f32 / effective_kws.len() as f32
};
```

Above the per-candidate loop (just after `let effective_names = idf_filter(...)`), add a one-time matcher precompile:

```rust
    let use_boundary = tunables::shrinkage_word_boundary_enabled();
    let kw_matchers: Vec<Regex> = if use_boundary {
        effective_kws.iter().map(|kw| compile_token_matcher(kw)).collect()
    } else {
        Vec::new()
    };
```

Replace the `kw_boost` computation inside the per-candidate loop with:

```rust
        let kw_boost = if effective_kws.is_empty() {
            0.0
        } else if use_boundary {
            let hits = kw_matchers
                .iter()
                .filter(|m| token_hit(&doc, m))
                .count();
            hits as f32 / effective_kws.len() as f32
        } else {
            let hits = effective_kws
                .iter()
                .filter(|kw| doc.contains(kw.as_str()))
                .count();
            hits as f32 / effective_kws.len() as f32
        };
```

Leave `name_boost` and `quoted_boost` paths untouched (Task 4 wires names; quoted is intentionally unchanged).

- [ ] **Step 4: Run the integration tests; expect them to pass**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test`
Expected: all four tests pass.

- [ ] **Step 5: Run the full rerank module tests**

Run: `cargo test -p ironmem --lib search::rerank`
Expected: all green. The existing `test_shrinkage_lifts_evidence_match` and `test_no_panic_on_empty` tests still pass.

- [ ] **Step 6: Run the full ironmem suite**

Run: `cargo test -p ironmem`
Expected: all green. Pay attention to any other tests that might depend on the old substring behavior (none expected per the spec, but verify).

- [ ] **Step 7: Commit**

```bash
git add crates/ironmem/src/search/rerank.rs crates/ironmem/tests/shrinkage_word_boundary_test.rs
git commit -m "feat(rerank): word-boundary matching for kw_boost path"
```

---

## Task 4: Wire helpers into `shrinkage_rerank`'s `name_boost` path

**Files:**
- Modify: `crates/ironmem/src/search/rerank.rs`
- Modify: `crates/ironmem/tests/shrinkage_word_boundary_test.rs`

- [ ] **Step 1: Append the failing test for the name path**

Add to the bottom of `crates/ironmem/tests/shrinkage_word_boundary_test.rs`:

```rust
#[test]
fn boundary_name_does_not_match_unrelated_substring() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");

    // Name "Sam" must NOT inflate a drawer whose content has only "sample".
    let mut cs = vec![
        make("Sam attended the meeting at noon", 0.50),
        make("a sample of the data was collected", 0.50),
    ];
    let signals = extract_signals("did Sam attend?");
    shrinkage_rerank(&mut cs, &signals);

    let sam_score = cs
        .iter()
        .find(|c| c.drawer.content.starts_with("Sam attended"))
        .map(|c| c.score)
        .unwrap();
    let sample_score = cs
        .iter()
        .find(|c| c.drawer.content.starts_with("a sample"))
        .map(|c| c.score)
        .unwrap();

    assert!(
        sam_score > sample_score,
        "name 'Sam' must not substring-match 'sample' under boundary mode ({sam_score} vs {sample_score})"
    );

    std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
}
```

- [ ] **Step 2: Run the test; expect it to fail**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test boundary_name_does_not_match_unrelated_substring`
Expected: FAIL — the `name_boost` branch in `shrinkage_rerank` still uses `doc.contains(name.to_lowercase().as_str())`, which substring-matches "sam" → "sample".

- [ ] **Step 3: Modify `name_boost` in `shrinkage_rerank` to thread the tunable**

In `crates/ironmem/src/search/rerank.rs::shrinkage_rerank`, alongside the `kw_matchers` precompile added in Task 3, also precompile name matchers:

```rust
    let name_matchers: Vec<Regex> = if use_boundary {
        effective_names
            .iter()
            .map(|n| compile_token_matcher(&n.to_lowercase()))
            .collect()
    } else {
        Vec::new()
    };
```

Then replace the `name_boost` computation inside the per-candidate loop. Find this block:

```rust
        let name_boost = if effective_names.is_empty() {
            0.0
        } else {
            let hits = effective_names
                .iter()
                .filter(|n| doc.contains(n.to_lowercase().as_str()))
                .count();
            hits as f32 / effective_names.len() as f32
        };
```

Replace with:

```rust
        let name_boost = if effective_names.is_empty() {
            0.0
        } else if use_boundary {
            let hits = name_matchers
                .iter()
                .filter(|m| token_hit(&doc, m))
                .count();
            hits as f32 / effective_names.len() as f32
        } else {
            let hits = effective_names
                .iter()
                .filter(|n| doc.contains(n.to_lowercase().as_str()))
                .count();
            hits as f32 / effective_names.len() as f32
        };
```

- [ ] **Step 4: Run the new test; expect it to pass**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test boundary_name_does_not_match_unrelated_substring`
Expected: PASS.

- [ ] **Step 5: Run the full rerank module + integration tests**

Run: `cargo test -p ironmem --lib search::rerank && cargo test -p ironmem --test shrinkage_word_boundary_test`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/ironmem/src/search/rerank.rs crates/ironmem/tests/shrinkage_word_boundary_test.rs
git commit -m "feat(rerank): word-boundary matching for name_boost path"
```

---

## Task 5: Wire helpers into `idf_filter` (symmetric matcher)

**Files:**
- Modify: `crates/ironmem/src/search/rerank.rs`
- Modify: `crates/ironmem/tests/shrinkage_word_boundary_test.rs`

- [ ] **Step 1: Append the failing test for IDF filter symmetry**

Add to `crates/ironmem/tests/shrinkage_word_boundary_test.rs`:

```rust
#[test]
fn idf_filter_uses_same_matcher_as_scorer() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");

    // Setup: 10 candidates, every one contains "suggestions" in its body
    // (a noun form). Under boundary mode with suffix tolerance the token
    // "suggest" hits all 10 — DF=10/10=100% which exceeds the 80% IDF
    // threshold and `idf_filter` must drop "suggest" from effective_kws,
    // so no boost gets applied. Score parity preserved.
    //
    // Under legacy substring (which substring-matches "suggest" against
    // "suggestions" identically) the IDF count would also be 10/10 — BUT
    // we are testing the symmetric coupling, not boundary's effect alone.
    // Construct a query whose token would hit ≥80% under boundary but only
    // <80% under substring? That's only possible if substring under-counts
    // — which is exactly the photography bug. Use a token where boundary
    // counts MORE strictly (matches fewer drawers) than substring.

    // Simpler scenario: 10 drawers each containing "currents". Token
    // "current" under boundary matches all 10 (suffix tolerance with 's').
    // Under substring it matches all 10 too. Both paths agree → IDF
    // filters identically. This test asserts that property directly: a
    // boost-able token disappears from effective_kws when its DF crosses
    // the 80% threshold under both modes.
    let mut cs: Vec<ScoredDrawer> = (0..10)
        .map(|i| make(&format!("we have currents in case {i}"), 0.50))
        .collect();
    let signals = extract_signals("the current state of things");
    shrinkage_rerank(&mut cs, &signals);

    // Under boundary mode with IDF filter symmetric: "current" matches
    // "currents" in 10/10 candidates → DF=100% > 80% → filtered out by
    // idf_filter → no kw_boost applied → all candidates retain score 0.50.
    for c in &cs {
        assert!(
            (c.score - 0.50).abs() < 1e-6,
            "score should be unchanged when token is IDF-filtered, got {}",
            c.score
        );
    }

    std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
}
```

- [ ] **Step 2: Run the test; expect it to pass IF and ONLY IF the IDF filter is symmetric**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test idf_filter_uses_same_matcher_as_scorer`

Expected: PASS if the IDF filter already happens to count identically (boundary-mode "current" with `s` suffix tolerance matches "currents" the same way substring "current" matches "currents"). FAIL if the IDF filter under-counts and lets "current" through, then the scorer (boundary mode) boosts every candidate.

If the test passes already, that's because the existing IDF filter coincidentally agrees with boundary-mode counting in this scenario. The test still has value because it locks in the symmetric-counting property. Proceed to step 3 to make the symmetry explicit (the spec requires both call sites to use the same matcher, regardless of whether they happen to agree on a given case).

- [ ] **Step 3: Modify `idf_filter` to use the same matcher**

In `crates/ironmem/src/search/rerank.rs`, locate `idf_filter` (currently at line 296). Replace its body:

```rust
fn idf_filter(tokens: &[String], candidates: &[ScoredDrawer], threshold: usize) -> Vec<String> {
    tokens
        .iter()
        .filter(|t| {
            let t_lower = t.to_lowercase();
            let df = candidates
                .iter()
                .filter(|c| c.drawer.content.to_lowercase().contains(t_lower.as_str()))
                .count();
            df < threshold
        })
        .cloned()
        .collect()
}
```

with:

```rust
fn idf_filter(tokens: &[String], candidates: &[ScoredDrawer], threshold: usize) -> Vec<String> {
    let use_boundary = tunables::shrinkage_word_boundary_enabled();
    tokens
        .iter()
        .filter(|t| {
            let t_lower = t.to_lowercase();
            let df = if use_boundary {
                let m = compile_token_matcher(&t_lower);
                candidates
                    .iter()
                    .filter(|c| m.is_match(&c.drawer.content.to_lowercase()))
                    .count()
            } else {
                candidates
                    .iter()
                    .filter(|c| c.drawer.content.to_lowercase().contains(t_lower.as_str()))
                    .count()
            };
            df < threshold
        })
        .cloned()
        .collect()
}
```

- [ ] **Step 4: Run the integration test suite**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test`
Expected: all 5 tests pass (the 4 from Tasks 3-4 plus the new IDF symmetry test).

- [ ] **Step 5: Run the full ironmem suite to confirm no regression**

Run: `cargo test -p ironmem`
Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add crates/ironmem/src/search/rerank.rs crates/ironmem/tests/shrinkage_word_boundary_test.rs
git commit -m "feat(rerank): symmetric word-boundary matcher in idf_filter"
```

---

## Task 6: Quoted-phrase regression test (intentionally unchanged)

**Files:**
- Modify: `crates/ironmem/tests/shrinkage_word_boundary_test.rs`

This task locks in the spec's "quoted phrases keep substring" non-goal. No production code changes; just a test that asserts quoted-phrase behavior is identical in both modes.

- [ ] **Step 1: Append the test**

Add to `crates/ironmem/tests/shrinkage_word_boundary_test.rs`:

```rust
#[test]
fn quoted_phrase_unchanged_in_both_modes() {
    let signals = extract_signals(r#"What did she call "the project"?"#);
    let p_drawer = make("she discussed the project last week", 0.50);
    let unrelated = make("totally unrelated meeting notes", 0.50);

    // Run under boundary mode
    {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");
        let mut cs = vec![p_drawer.clone(), unrelated.clone()];
        shrinkage_rerank(&mut cs, &signals);
        let project_score = cs
            .iter()
            .find(|c| c.drawer.content.starts_with("she discussed"))
            .map(|c| c.score)
            .unwrap();
        let unrelated_score = cs
            .iter()
            .find(|c| c.drawer.content.starts_with("totally unrelated"))
            .map(|c| c.score)
            .unwrap();
        assert!(
            project_score > unrelated_score,
            "boundary mode: quoted phrase 'the project' must lift the relevant drawer"
        );
        std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
    }

    // Same under legacy substring mode
    {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "0");
        let mut cs = vec![p_drawer.clone(), unrelated.clone()];
        shrinkage_rerank(&mut cs, &signals);
        let project_score = cs
            .iter()
            .find(|c| c.drawer.content.starts_with("she discussed"))
            .map(|c| c.score)
            .unwrap();
        let unrelated_score = cs
            .iter()
            .find(|c| c.drawer.content.starts_with("totally unrelated"))
            .map(|c| c.score)
            .unwrap();
        assert!(
            project_score > unrelated_score,
            "legacy mode: quoted phrase 'the project' must lift the relevant drawer"
        );
        std::env::remove_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY");
    }
}
```

- [ ] **Step 2: Run the test; expect it to pass without code changes**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test quoted_phrase_unchanged_in_both_modes`
Expected: PASS in both branches. The quoted-phrase scoring path was never modified, so it lifts the relevant drawer identically in both modes.

- [ ] **Step 3: Run the full integration suite**

Run: `cargo test -p ironmem --test shrinkage_word_boundary_test`
Expected: all 6 tests green (4 from Task 3, 1 from Task 4, 1 from Task 5, 1 from this task — counted by behavior, not by spec ordering).

- [ ] **Step 4: Lint**

Run: `cargo fmt --all && cargo clippy --workspace -- -D warnings`
Expected: no warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/ironmem/tests/shrinkage_word_boundary_test.rs
git commit -m "test(rerank): quoted-phrase regression — unchanged in both matcher modes"
```

---

## Task 7: Bench acceptance verification

**Files:** none (read-only verification using existing harness and existing OFF baseline).

The OFF baseline was captured in the pref-enrich experiment retrospective: 30 preference questions in `/tmp/pref_off.json` (gold_rank computed 1-indexed). The bench's per-question JSON contains `question_type`, `answer_sids`, `gold_rank`, `top10`, etc. We re-use it for the OFF side of the comparison.

- [ ] **Step 1: Verify the OFF baseline file exists**

Run: `ls -la /tmp/pref_off.json && head -1 /tmp/pref_off.json | python3 -m json.tool | head -10`
Expected: file exists; first line is a JSON object with `question_type`, `gold_rank`, `top10`, etc.

If the file is missing (e.g., a fresh machine), regenerate it:

```bash
cd /Users/jeffreycrum/git-repos/ironrace-memory
IRONMEM_SHRINKAGE_WORD_BOUNDARY=0 IRONMEM_EMBED_MODE=real \
  python3 scripts/benchmark_longmemeval.py /tmp/lme_pref_only.json \
  --limit 30 --ironmem-binary target/release/ironmem \
  --db-cache-dir /tmp/bench_pref_off \
  --per-question-json /tmp/pref_off.json
```

If `/tmp/lme_pref_only.json` is also missing, regenerate it from the LME cache:

```bash
python3 -c "
import json
data = json.load(open('/Users/jeffreycrum/.cache/ironrace/longmemeval_s'))
pref = [e for e in data if e.get('question_type') == 'single-session-preference']
with open('/tmp/lme_pref_only.json', 'w') as f:
    json.dump(pref, f)
print(f'wrote {len(pref)} preference questions')
"
```

- [ ] **Step 2: Build the release binary**

Run: `cd /Users/jeffreycrum/git-repos/ironrace-memory && cargo build --release -p ironmem`
Expected: clean release build.

- [ ] **Step 3: Run the bench with word-boundary ON (default)**

```bash
cd /Users/jeffreycrum/git-repos/ironrace-memory
rm -rf /tmp/bench_word_boundary_on
IRONMEM_SHRINKAGE_WORD_BOUNDARY=1 IRONMEM_EMBED_MODE=real \
  python3 scripts/benchmark_longmemeval.py /tmp/lme_pref_only.json \
  --limit 30 --ironmem-binary target/release/ironmem \
  --db-cache-dir /tmp/bench_word_boundary_on \
  --per-question-json /tmp/pref_word_boundary.json \
  > /tmp/bench_word_boundary_log.txt 2>&1
```

Expected: bench runs ~5 minutes (30 questions × ~50 sessions × <1s/session for ingest+search). Captured log ends with the per-type breakdown table.

- [ ] **Step 4: Compute the deltas**

```bash
python3 - <<'PY'
import json
from collections import defaultdict

def L(p): return [json.loads(l) for l in open(p) if l.strip()]
off = L('/tmp/pref_off.json')
on  = L('/tmp/pref_word_boundary.json')

def tally(rs):
    bt = defaultdict(lambda: {'n': 0, 'r@5': 0, 'r@10': 0})
    for r in rs:
        t = r.get('question_type', '?')
        gr = r.get('gold_rank')
        bt[t]['n'] += 1
        for k in (5, 10):
            if gr is not None and 1 <= gr <= k:
                bt[t][f'r@{k}'] += 1
    return {t: {f'r@{k}': v[f'r@{k}']/v['n']*100 for k in (5, 10)} | {'n': v['n']}
            for t, v in bt.items()}

o = tally(off); n = tally(on)
print('Per-type R@5 deltas (ON - OFF):')
for cat in sorted(set(o) | set(n)):
    a5 = n.get(cat, {}).get('r@5', 0); b5 = o.get(cat, {}).get('r@5', 0)
    n_off = o.get(cat, {}).get('n', 0); n_on = n.get(cat, {}).get('n', 0)
    print(f'  {cat:35s} OFF={b5:5.1f}%  ON={a5:5.1f}%  delta={a5-b5:+.2f}pp  (n={n_off}/{n_on})')
PY
```

Expected output format:
```
Per-type R@5 deltas (ON - OFF):
  single-session-preference           OFF= 70.0%  ON= XX.X%  delta=+X.XXpp  (n=30/30)
  ...
```

- [ ] **Step 5: Check the acceptance criteria**

| Criterion | Target |
|---|---|
| Preference R@5 delta | ≥ +5.00pp |
| Any other category R@5 regression | not worse than −1.00pp |
| Median search latency delta | ≤ +5ms |

For latency, grep the bench log:

```bash
grep -E 'R@5=.*med_search' /tmp/bench_word_boundary_log.txt
```

Expected: a line like `[ 30/30]  R@5=XX.X%  med_search=YY.Yms`. Compare YY.Y to the OFF run's median (in `/tmp/bench_off_log.txt` if it still exists, or rerun OFF for an apples-to-apples median).

- [ ] **Step 6: If acceptance met, commit the empirical record**

```bash
git commit --allow-empty -m "$(cat <<'EOF'
chore(bench): word-boundary shrinkage rerank — preference R@5 delta

OFF (IRONMEM_SHRINKAGE_WORD_BOUNDARY=0):
  single-session-preference R@5 = 70.0%

ON (IRONMEM_SHRINKAGE_WORD_BOUNDARY=1, default):
  single-session-preference R@5 = X.X%

Delta: +X.Xpp (acceptance: >= +5pp)

Other category R@5 deltas (limit=30, IRONMEM_RERANK unset):
  multi-session         delta=±X.Xpp
  single-session-user   delta=±X.Xpp

Median search latency: OFF=Yms ON=Yms (delta +Y.Yms; acceptance: <= +5ms)
EOF
)"
```

Replace placeholders with the actual measured numbers.

- [ ] **Step 7: If acceptance NOT met, do NOT commit. Report findings.**

Common failure modes and what they mean:

- **Preference R@5 delta < +5pp but > 0pp:** the matcher is fixing some cases but not enough. The most likely residual is `"suggest" → "suggestions"` (in the suffix list — see spec Risks). Capture per-question deltas and report DONE_WITH_CONCERNS with a hypothesis (likely: the residual case needs verb stop-listing as a follow-up spec).
- **Preference R@5 delta = 0pp or negative:** the matcher isn't behaving as designed. Re-run a single-question diagnostic against the photography query:
  ```bash
  python3 -u /tmp/probe_stages.py /tmp/bench_cache_off/<the photography Q's DB>.sqlite3 \
    "Can you suggest some accessories that would complement my current photography setup?" \
    "1e2176863a8961f4a27c52d59500392a"
  ```
  Verify the gold parent ranks #1 in RUN A under boundary mode. If it doesn't, the matcher isn't being threaded correctly — re-check Task 3-5 wiring.
- **Other category regresses by >1pp:** the suffix tolerance may be too narrow for that category's queries. Capture which questions regressed and report.

---

## Self-review

| Spec section | Plan task |
|---|---|
| New tunable `shrinkage_word_boundary_enabled()` | Task 1 |
| `compile_token_matcher` + `token_hit` helpers | Task 2 |
| Threading tunable through `kw_boost` | Task 3 |
| Threading tunable through `name_boost` | Task 4 |
| Threading tunable through `idf_filter` (symmetry) | Task 5 |
| Quoted-phrase unchanged regression test | Task 6 |
| Bench acceptance: ≥+5pp preference R@5, no >−1pp regression, ≤+5ms latency | Task 7 |
| 8 integration tests | Tasks 3 (4 tests), 4 (1 test), 5 (1 test), 6 (1 test) = 7 tests; the 8th case from the spec ("inflected forms still match") is covered by Task 2's `token_matcher_inflected_forms_match` unit test in the rerank inline tests |

**Test count reconciliation:** the spec listed 8 named tests; the plan reorganizes them into 7 integration tests (covering tasks 3-6) plus 7 unit tests in the inline `mod tests` (Task 2). The substantive coverage matches the spec; the inline-vs-integration split is an implementation detail that follows TDD-by-task structure.

**Type consistency check:**
- `compile_token_matcher(token: &str) -> Regex` — same signature in Tasks 2, 3, 4, 5.
- `token_hit(doc_lower: &str, matcher: &Regex) -> bool` — same in Tasks 2, 3, 4.
- `shrinkage_word_boundary_enabled() -> bool` — same in Tasks 1, 3, 4, 5.
- `make(content, score) -> ScoredDrawer` test helper — same shape in Tasks 3, 4, 5, 6.

**Placeholder scan:** no TBDs, no "implement appropriately" stubs, no "similar to Task N" hand-waves. Every code step shows the code; every command step shows the command; every commit message is concrete.

**Verified at plan time:**
- `regex` crate is at `crates/ironmem/Cargo.toml:40` — no new deps needed.
- `pub fn extract_signals` and `pub fn shrinkage_rerank` are public (`crates/ironmem/src/search/rerank.rs:175,226`).
- `Drawer` and `ScoredDrawer` are re-exported from `ironmem::db` (per the merge work earlier in this branch — `crates/ironmem/src/db/mod.rs`).
- `pref_enrich_enabled()` in `tunables.rs` uses the per-call (non-OnceLock) pattern Task 1 mirrors — engineer can read it for reference.
- The static `ENV_LOCK: Mutex<()>` pattern used in Tasks 3-6 follows
  `crates/ironmem/tests/preference_enrichment_test.rs` precedent — including the `unwrap_or_else(|e| e.into_inner())` for poison recovery.

**Risks acknowledged in spec:**
- Suffix tolerance still allows `"suggest" → "suggestions"`. Plan acceptance set at +5pp (not full closure) — calibrated to this known limitation. Task 7 step 7 instructs the engineer how to triage if the bar isn't cleared.
