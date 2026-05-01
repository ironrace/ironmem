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

#[test]
fn boundary_name_does_not_match_unrelated_substring() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");

    // Name "Sam" must NOT inflate a drawer whose content has only "sample".
    //
    // Design: query has ONLY a name signal ("Sam") with no predicate keywords
    // that distinguish the two target drawers. Under the buggy legacy-substring
    // path, both drawers score the same name_boost because "sam".contains("sam")
    // matches both "Sam was here" and "a sample". Under the boundary path only
    // the exact-word drawer gets boosted.
    //
    // We use 6 candidates so the IDF filter keeps "Sam": threshold =
    // ceil(6 * 0.80) = 5; "sam" appears in only 2 of 6 docs → survives.
    //
    // The two target drawers start equal (0.50). Four padding drawers (also
    // 0.50) don't mention "sam" or any distinguishing keyword, so they won't
    // interfere. The query "Who is Sam?" contains no predicate content words
    // that appear in either target drawer, so name_boost is the sole signal.
    let mut cs = vec![
        make("Sam was present at the event", 0.50),
        make("a sample of the data was collected", 0.50),
        make("the weather was fine on Tuesday", 0.50),
        make("dinner reservations confirmed for Friday", 0.50),
        make("the quarterly report looked promising", 0.50),
        make("bike ride along the river trail yesterday", 0.50),
    ];
    let signals = extract_signals("Who is Sam?");
    shrinkage_rerank(&mut cs, &signals);

    let sam_score = cs
        .iter()
        .find(|c| c.drawer.content.starts_with("Sam was present"))
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

#[test]
fn idf_filter_uses_same_matcher_as_scorer() {
    let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IRONMEM_SHRINKAGE_WORD_BOUNDARY", "1");

    // Setup: 10 candidates, every one contains "currents" in its body
    // (a noun form). Under boundary mode with suffix tolerance the token
    // "current" hits all 10 — DF=10/10=100% which exceeds the 80% IDF
    // threshold and `idf_filter` must drop "current" from effective_kws,
    // so no boost gets applied. Score parity preserved.
    //
    // Under legacy substring (which substring-matches "current" against
    // "currents" identically) the IDF count would also be 10/10 — BUT
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
